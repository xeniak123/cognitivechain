//! Mining pool: turns one miner's all-or-nothing lottery into a steady income.
//!
//! # Why a pool needs more than share counting here
//!
//! In a hash-based chain a share is self-proving: the same hash that might win a
//! block also proves the work, so a pool verifies it in microseconds. Here the
//! work is a matrix product and the proof-of-work hash says nothing about
//! whether that product was ever computed. A miner could submit random Merkle
//! roots with random nonces and collect shares for free.
//!
//! So the pool applies the same commit-reveal spot-check the chain does, at a
//! smaller scale: every share is challenged on [`SHARE_CHECK_ROWS`] randomly
//! chosen rows, which the pool recomputes itself. The challenge is drawn after
//! the share arrives and from the pool's own randomness, so it cannot be
//! predicted. A miner computing only a fraction `f` of the rows passes one
//! challenge with probability `f^4`, and [`MAX_STRIKES`] failures ban the
//! address entirely, forfeiting its unpaid credit.
//!
//! # Accounting
//!
//! Rewards are attributed from the chain, not from the pool's balance: the pool
//! scans canonical blocks for ones it mined whose commitment was opened in the
//! following block, and credits exactly that block's reward plus its fees. This
//! keeps accounting independent of payouts in flight, which a balance-watching
//! scheme cannot manage without racing itself.
//!
//! Payment is PPLNS over the last [`PoolConfig::pplns_window`] verified shares:
//! a share earns from every block found while it is still inside the window,
//! which makes hopping between pools unprofitable without punishing anyone who
//! simply mines steadily.

use crate::client::rpc_call;
use crate::crypto::Keypair;
use crate::pouw;
use crate::types::{format_cog, meets_difficulty, Address, Hash};
use anyhow::{anyhow, bail, Context, Result};
use axum::extract::State as AxumState;
use axum::routing::get;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Rows recomputed per share. Four keeps verification near 4 ms while making
/// half-done work a 1-in-256 gamble that is repeated on every single share.
pub const SHARE_CHECK_ROWS: usize = 4;
/// Failed spot-checks tolerated before an address is banned.
pub const MAX_STRIKES: u32 = 3;
/// A share whose spot-check is never answered is dropped after this long.
const SHARE_TIMEOUT_SECS: u64 = 120;
/// Payouts per round. The mempool tolerates only a limited forward nonce gap.
const MAX_PAYOUTS_PER_ROUND: usize = 12;

#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub node: String,
    pub bind: SocketAddr,
    /// Percentage the pool keeps, 0.0 to 100.0.
    pub fee_percent: f64,
    /// Difficulty a share must meet. Lower means more frequent, smaller shares.
    pub share_difficulty: u64,
    /// How many recent verified shares split each block reward.
    pub pplns_window: usize,
    /// Credit below which a payout is not worth its own transaction fee.
    pub min_payout: u64,
    /// Fee attached to payout transactions.
    pub payout_fee: u64,
    pub state_path: PathBuf,
}

/// A share awaiting its spot-check.
#[derive(Clone, Debug)]
struct PendingShare {
    payout: Address,
    seed: Hash,
    matmul_root: Hash,
    rows: Vec<u32>,
    created: u64,
}

/// The part of the pool's state that must survive a restart. Losing it would
/// lose miners their unpaid earnings, so it is written on every change.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    /// Unpaid balance owed to each miner, in acog.
    pub credits: BTreeMap<Address, u64>,
    /// Lifetime total already sent to each miner.
    pub paid: BTreeMap<Address, u64>,
    /// Failed spot-checks per address; `MAX_STRIKES` means banned.
    pub strikes: BTreeMap<Address, u32>,
    /// Highest canonical height already examined for settled rewards.
    pub last_scanned_height: u64,
    pub blocks_found: u64,
    pub shares_accepted: u64,
    pub shares_rejected: u64,
    /// Lifetime fee kept by the pool operator.
    pub fees_earned: u64,
}

pub struct PoolState {
    pub ledger: Ledger,
    /// Verified shares, newest last. This is the PPLNS window.
    shares: VecDeque<(Address, u64)>,
    pending: HashMap<Hash, PendingShare>,
    /// Challenges the node wants answered, and who can answer them.
    node_requests: HashMap<Hash, (Vec<u32>, Address)>,
    /// Which miner found each block the pool submitted.
    found_by: HashMap<Hash, Address>,
    /// Current chain tip as seen by the pool.
    tip: Hash,
    height: u64,
    network_difficulty: u64,
    block_reward: u64,
    /// Chain id reported by the node; part of what payouts sign over.
    chain_id: String,
    connected: bool,
}

#[derive(Clone)]
pub struct Pool {
    pub config: PoolConfig,
    pub keypair: Arc<Keypair>,
    pub address: Address,
    pub state: Arc<Mutex<PoolState>>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Pool {
    pub fn new(config: PoolConfig, keypair: Keypair) -> Result<Self> {
        let address = keypair.address()?;
        let ledger = if config.state_path.exists() {
            let raw = std::fs::read_to_string(&config.state_path).with_context(|| {
                format!("cannot read pool ledger {}", config.state_path.display())
            })?;
            serde_json::from_str(&raw).with_context(|| {
                format!(
                    "cannot parse pool ledger {}; refusing to start rather than \
                     lose what miners are owed",
                    config.state_path.display()
                )
            })?
        } else {
            Ledger::default()
        };

        Ok(Pool {
            config,
            keypair: Arc::new(keypair),
            address,
            state: Arc::new(Mutex::new(PoolState {
                ledger,
                shares: VecDeque::new(),
                pending: HashMap::new(),
                node_requests: HashMap::new(),
                found_by: HashMap::new(),
                tip: [0u8; 32],
                height: 0,
                network_difficulty: u64::MAX,
                block_reward: 0,
                chain_id: String::new(),
                connected: false,
            })),
        })
    }

    /// Persist the ledger by writing a temporary file and renaming it, so an
    /// interrupted write can never leave a half-written ledger behind.
    fn save(&self, ledger: &Ledger) {
        let tmp = self.config.state_path.with_extension("tmp");
        let encoded = match serde_json::to_string_pretty(ledger) {
            Ok(v) => v,
            Err(err) => {
                tracing::error!("cannot serialise pool ledger: {err}");
                return;
            }
        };
        if let Err(err) = std::fs::write(&tmp, encoded) {
            tracing::error!("cannot write pool ledger: {err}");
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, &self.config.state_path) {
            tracing::error!("cannot replace pool ledger: {err}");
        }
    }

    /// Distribute one block's proceeds across the PPLNS window.
    fn credit_block(&self, state: &mut PoolState, gross: u64, height: u64) {
        let fee = ((gross as f64) * self.config.fee_percent / 100.0) as u64;
        let net = gross.saturating_sub(fee);
        let total_weight: u64 = state.shares.iter().map(|(_, w)| *w).sum();

        if total_weight == 0 || net == 0 {
            // Nobody has a live share, so the operator keeps it rather than
            // burning it. This only happens on a pool with no miners.
            state.ledger.fees_earned = state.ledger.fees_earned.saturating_add(gross);
            tracing::warn!("block at height {height} settled with no shares in the window");
            return;
        }

        let mut per_address: BTreeMap<Address, u64> = BTreeMap::new();
        for (addr, weight) in &state.shares {
            *per_address.entry(*addr).or_insert(0) += *weight;
        }

        let mut distributed = 0u64;
        for (addr, weight) in &per_address {
            // u128 keeps the product exact for any reward inside the supply cap.
            let cut = ((net as u128) * (*weight as u128) / (total_weight as u128)) as u64;
            if cut == 0 {
                continue;
            }
            *state.ledger.credits.entry(*addr).or_insert(0) += cut;
            distributed += cut;
        }
        // Integer division leaves a remainder; it goes to the operator rather
        // than silently vanishing from the ledger.
        let remainder = net.saturating_sub(distributed);
        state.ledger.fees_earned = state
            .ledger
            .fees_earned
            .saturating_add(fee)
            .saturating_add(remainder);
        state.ledger.blocks_found += 1;

        tracing::info!(
            "block at height {height} settled: {} COG split across {} miners ({} COG fee)",
            format_cog(distributed),
            per_address.len(),
            format_cog(fee + remainder)
        );
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC surface. Deliberately the same method names as the node, so an
// unmodified miner points at a pool by changing one flag.
// ---------------------------------------------------------------------------

pub async fn serve(pool: Pool) -> Result<()> {
    let bind = pool.config.bind;
    let app = Router::new()
        .route("/", get(overview).post(handle_rpc))
        .route("/health", get(|| async { "ok" }))
        .with_state(pool);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("cannot bind pool on {bind}"))?;
    tracing::info!("pool listening on http://{bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn overview(AxumState(pool): AxumState<Pool>) -> axum::response::Html<String> {
    let state = pool.state.lock();
    let miners: Vec<String> = state
        .ledger
        .credits
        .iter()
        .map(|(a, c)| format!("{a}  {} COG", format_cog(*c)))
        .collect();
    axum::response::Html(format!(
        "<!doctype html><meta charset=utf-8><title>Pula CognitiveChain</title>\
         <body style=\"font-family:ui-monospace,monospace;max-width:70ch;margin:3rem auto;padding:0 1rem\">\
         <h1>Pula CognitiveChain</h1>\
         <p>Adres puli: {}</p><p>Prowizja: {} %</p>\
         <p>Trudność udziału: {}</p><p>Wysokość: {}</p>\
         <p>Znalezione bloki: {}</p><p>Udziały: {} przyjętych, {} odrzuconych</p>\
         <h2>Kop tutaj</h2><pre>cog-miner --wallet &lt;TWÓJ_ADRES&gt; --pool {}</pre>\
         <h2>Nierozliczone salda</h2><pre>{}</pre></body>",
        pool.address,
        pool.config.fee_percent,
        pool.config.share_difficulty,
        state.height,
        state.ledger.blocks_found,
        state.ledger.shares_accepted,
        state.ledger.shares_rejected,
        pool.config.bind,
        if miners.is_empty() {
            "(brak)".to_string()
        } else {
            miners.join("\n")
        }
    ))
}

async fn handle_rpc(AxumState(pool): AxumState<Pool>, Json(req): Json<Value>) -> Json<Value> {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let params = req.get("params").cloned().unwrap_or(json!({}));

    let result = dispatch(&pool, &method, &params).await;
    match result {
        Ok(value) => Json(json!({"jsonrpc": "2.0", "id": id, "result": value})),
        Err(err) => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32000, "message": format!("{err:#}")}
        })),
    }
}

fn addr_param(params: &Value, key: &str) -> Result<Address> {
    let raw = params
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing string parameter `{key}`"))?;
    Address::parse(raw).map_err(|e| anyhow!("`{key}`: {e}"))
}

fn hash_param(params: &Value, key: &str) -> Result<Hash> {
    let raw = hex::decode(
        params
            .get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing string parameter `{key}`"))?,
    )?;
    if raw.len() != 32 {
        bail!("`{key}` must be a 32-byte hex string");
    }
    let mut h = [0u8; 32];
    h.copy_from_slice(&raw);
    Ok(h)
}

fn u64_param(params: &Value, key: &str) -> Result<u64> {
    match params.get(key) {
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| anyhow!("`{key}` must be a non-negative integer")),
        Some(Value::String(s)) => s
            .parse()
            .map_err(|_| anyhow!("`{key}` must be an integer, got {s:?}")),
        _ => Err(anyhow!("missing integer parameter `{key}`")),
    }
}

/// Rows this share must open. Drawn from the pool's own randomness *after* the
/// share arrives, so a miner cannot know them while choosing what to compute.
fn draw_challenge() -> Vec<u32> {
    let seed: [u8; 32] = rand::random();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cog/pool-share/v1");
    hasher.update(&seed);
    let mut xof = hasher.finalize_xof();
    let mut bytes = vec![0u8; 4 * SHARE_CHECK_ROWS];
    xof.fill(&mut bytes);
    (0..SHARE_CHECK_ROWS)
        .map(|i| {
            let mut b = [0u8; 4];
            b.copy_from_slice(&bytes[4 * i..4 * i + 4]);
            u32::from_le_bytes(b) % pouw::N as u32
        })
        .collect()
}

async fn dispatch(pool: &Pool, method: &str, params: &Value) -> Result<Value> {
    match method {
        // The miner gets the node's work, but told to compute under the pool's
        // address and to a lower difficulty.
        "cog_getWork" => {
            let state = pool.state.lock();
            if !state.connected {
                bail!("pool has not reached its node yet");
            }
            Ok(json!({
                "chain_id": "pool",
                "height": state.height + 1,
                "prev_hash": hex::encode(state.tip),
                "difficulty": pool.config.share_difficulty,
                "network_difficulty": state.network_difficulty,
                "mining_address": pool.address.to_hex(),
                "matrix_dim": pouw::N,
                "field_prime": pouw::P,
                "max_nonce": pouw::MAX_NONCE,
                "challenge_rows": pouw::CHALLENGE_ROWS,
            }))
        }

        "cog_submitSolution" => {
            let payout = addr_param(params, "miner")?;
            let salt = u64_param(params, "salt")?;
            let nonce = u64_param(params, "nonce")?;
            let matmul_root = hash_param(params, "matmul_root")?;

            if nonce >= pouw::MAX_NONCE {
                bail!("nonce out of range");
            }

            let (tip, network_difficulty) = {
                let state = pool.state.lock();
                if state.ledger.strikes.get(&payout).copied().unwrap_or(0) >= MAX_STRIKES {
                    bail!("this address is banned for repeatedly failing share verification");
                }
                (state.tip, state.network_difficulty)
            };

            // The seed uses the POOL's address, because that is what will end up
            // in the block if this share happens to win one.
            let seed = pouw::task_seed(&tip, &pool.address, salt);
            let pow = pouw::pow_hash(&seed, &matmul_root, nonce);
            if !meets_difficulty(&pow, pool.config.share_difficulty) {
                let mut state = pool.state.lock();
                state.ledger.shares_rejected += 1;
                bail!("share does not meet the pool difficulty");
            }
            let share_id = pouw::commit_id(&seed, &matmul_root, nonce);
            let is_block = meets_difficulty(&pow, network_difficulty);

            {
                let mut state = pool.state.lock();
                if state.pending.contains_key(&share_id) {
                    bail!("duplicate share");
                }
                state.pending.insert(
                    share_id,
                    PendingShare {
                        payout,
                        seed,
                        matmul_root,
                        rows: draw_challenge(),
                        created: now(),
                    },
                );
                if is_block {
                    state.found_by.insert(share_id, payout);
                    // Bound the map: only recent blocks can still owe a reveal.
                    if state.found_by.len() > 1024 {
                        let stale: Vec<Hash> = state
                            .found_by
                            .keys()
                            .filter(|k| !state.node_requests.contains_key(*k))
                            .take(512)
                            .copied()
                            .collect();
                        for k in stale {
                            state.found_by.remove(&k);
                        }
                    }
                }
            }

            // A winning share goes to the node immediately. Forwarding before the
            // spot-check finishes is safe: an invalid commitment simply fails at
            // the node and earns nothing, while a delay could lose a real block.
            let mut forwarded = false;
            if is_block {
                match rpc_call(
                    &pool.config.node,
                    "cog_submitSolution",
                    json!({
                        "miner": pool.address.to_hex(),
                        "salt": salt,
                        "nonce": nonce,
                        "matmul_root": hex::encode(matmul_root),
                    }),
                )
                .await
                {
                    Ok(v) => {
                        forwarded = v.get("status").and_then(|s| s.as_str()) == Some("accepted");
                        if forwarded {
                            tracing::info!(
                                "block found by {payout} at height {}",
                                v.get("height").and_then(|h| h.as_u64()).unwrap_or(0)
                            );
                        }
                    }
                    Err(err) => tracing::warn!("node rejected a forwarded block: {err:#}"),
                }
            }

            Ok(json!({
                "status": "accepted",
                "share_id": hex::encode(share_id),
                "block_candidate": is_block,
                "block_accepted": forwarded,
                "message": "share recorded; answer its verification request to have it counted",
            }))
        }

        // Both the pool's own spot-checks and any challenge the node is waiting
        // on for a block this miner found.
        "cog_getRevealRequests" => {
            let miner = addr_param(params, "miner")?;
            let state = pool.state.lock();
            let mut requests = Vec::new();
            for (id, share) in &state.pending {
                if share.payout == miner {
                    requests.push(json!({
                        "commit_id": hex::encode(id),
                        "task_seed": hex::encode(share.seed),
                        "rows": share.rows,
                        "purpose": "share",
                    }));
                }
            }
            for (id, (rows, finder)) in &state.node_requests {
                if *finder == miner {
                    requests.push(json!({
                        "commit_id": hex::encode(id),
                        "task_seed": hex::encode(id),
                        "rows": rows,
                        "purpose": "block",
                    }));
                }
            }
            Ok(json!({ "requests": requests }))
        }

        "cog_submitReveal" => {
            let commit_id = hash_param(params, "commit_id")?;
            let rows_json = params
                .get("rows")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("missing array parameter `rows`"))?;

            // A challenge the node is waiting on: pass it straight through. The
            // node performs the full check itself.
            let node_request = {
                let state = pool.state.lock();
                state.node_requests.contains_key(&commit_id)
            };
            if node_request {
                let result = rpc_call(
                    &pool.config.node,
                    "cog_submitReveal",
                    json!({ "commit_id": hex::encode(commit_id), "rows": rows_json }),
                )
                .await;
                let mut state = pool.state.lock();
                state.node_requests.remove(&commit_id);
                return match result {
                    Ok(_) => Ok(json!({"status": "accepted", "forwarded": true})),
                    Err(err) => Err(err),
                };
            }

            let share = {
                let state = pool.state.lock();
                state
                    .pending
                    .get(&commit_id)
                    .cloned()
                    .ok_or_else(|| anyhow!("unknown or already settled share"))?
            };

            let mut parsed = Vec::with_capacity(rows_json.len());
            for entry in rows_json {
                let index = u64_param(entry, "index")? as u32;
                let raw = hex::decode(
                    entry
                        .get("values")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow!("row {index}: missing `values`"))?,
                )?;
                if raw.len() != pouw::N * 2 {
                    bail!("row {index}: `values` must be {} hex bytes", pouw::N * 2);
                }
                let values: Vec<u16> = raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                let mut proof = Vec::new();
                for p in entry
                    .get("proof")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow!("row {index}: missing `proof`"))?
                {
                    let bytes = hex::decode(
                        p.as_str()
                            .ok_or_else(|| anyhow!("row {index}: proof entries must be hex"))?,
                    )?;
                    if bytes.len() != 32 {
                        bail!("row {index}: proof entries must be 32 bytes");
                    }
                    let mut h = [0u8; 32];
                    h.copy_from_slice(&bytes);
                    proof.push(h);
                }
                parsed.push((index, values, proof));
            }

            // Recomputing rows is CPU-bound; keep it off the async executor.
            let verdict = tokio::task::block_in_place(|| verify_share(&share, &parsed));

            let mut state = pool.state.lock();
            state.pending.remove(&commit_id);
            match verdict {
                Ok(()) => {
                    state.shares.push_back((share.payout, 1));
                    while state.shares.len() > pool.config.pplns_window {
                        state.shares.pop_front();
                    }
                    state.ledger.shares_accepted += 1;
                    let ledger = state.ledger.clone();
                    drop(state);
                    pool.save(&ledger);
                    Ok(json!({"status": "accepted", "counted": true}))
                }
                Err(err) => {
                    state.ledger.shares_rejected += 1;
                    let strikes = state.ledger.strikes.entry(share.payout).or_insert(0);
                    *strikes += 1;
                    let banned = *strikes >= MAX_STRIKES;
                    let count = *strikes;
                    let ledger = state.ledger.clone();
                    drop(state);
                    pool.save(&ledger);
                    if banned {
                        tracing::warn!("banning {} after {count} failed spot-checks", share.payout);
                    }
                    Err(anyhow!(
                        "share verification failed ({err}); strike {count} of {MAX_STRIKES}"
                    ))
                }
            }
        }

        // Miners check their pool credit here rather than their on-chain balance.
        "pool_getBalance" => {
            let miner = addr_param(params, "miner")?;
            let state = pool.state.lock();
            let credit = state.ledger.credits.get(&miner).copied().unwrap_or(0);
            let paid = state.ledger.paid.get(&miner).copied().unwrap_or(0);
            Ok(json!({
                "miner": miner.to_hex(),
                "unpaid_acog": credit.to_string(),
                "unpaid_cog": format_cog(credit),
                "paid_cog": format_cog(paid),
                "shares_in_window": state.shares.iter().filter(|(a, _)| *a == miner).count(),
                "strikes": state.ledger.strikes.get(&miner).copied().unwrap_or(0),
                "min_payout_cog": format_cog(pool.config.min_payout),
            }))
        }

        "cog_getBalance" => rpc_call(&pool.config.node, "cog_getBalance", params.clone()).await,

        "pool_getStats" => {
            let state = pool.state.lock();
            Ok(json!({
                "pool_address": pool.address.to_hex(),
                "fee_percent": pool.config.fee_percent,
                "share_difficulty": pool.config.share_difficulty,
                "network_difficulty": state.network_difficulty,
                "height": state.height,
                "connected": state.connected,
                "blocks_found": state.ledger.blocks_found,
                "shares_accepted": state.ledger.shares_accepted,
                "shares_rejected": state.ledger.shares_rejected,
                "shares_in_window": state.shares.len(),
                "pplns_window": pool.config.pplns_window,
                "miners_owed": state.ledger.credits.len(),
                "fees_earned_cog": format_cog(state.ledger.fees_earned),
            }))
        }

        other => Err(anyhow!("unknown method `{other}`")),
    }
}

/// Recompute the challenged rows and check them against the committed root.
fn verify_share(share: &PendingShare, rows: &[(u32, Vec<u16>, Vec<Hash>)]) -> Result<()> {
    if rows.len() != share.rows.len() {
        bail!("expected {} rows, got {}", share.rows.len(), rows.len());
    }
    let a = pouw::gen_matrix_a(&share.seed);
    let b = pouw::gen_matrix_b(&share.seed);

    for (slot, (index, values, proof)) in rows.iter().enumerate() {
        if *index != share.rows[slot] {
            bail!(
                "row {slot} opens {index}, challenge demanded {}",
                share.rows[slot]
            );
        }
        if values.len() != pouw::N {
            bail!("row {slot} has the wrong length");
        }
        let leaf = pouw::leaf_hash(*index, values);
        if !pouw::merkle_verify(&share.matmul_root, *index, &leaf, proof) {
            bail!("row {slot} fails its Merkle proof");
        }
        let i = *index as usize;
        if pouw::matmul_row(&a[i * pouw::N..i * pouw::N + pouw::N], &b) != *values {
            bail!("row {slot} does not match the recomputed product");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Background work
// ---------------------------------------------------------------------------

/// Track the chain tip and the node's outstanding challenges.
pub async fn follow_node(pool: Pool) {
    loop {
        match rpc_call(&pool.config.node, "cog_getWork", json!({})).await {
            Ok(work) => {
                let mut state = pool.state.lock();
                if let Some(prev) = work.get("prev_hash").and_then(|v| v.as_str()) {
                    if let Ok(raw) = hex::decode(prev) {
                        if raw.len() == 32 {
                            let mut h = [0u8; 32];
                            h.copy_from_slice(&raw);
                            // Pending shares deliberately survive a tip change:
                            // they are verified against the seed stored with the
                            // share, so work already done still counts. Stale
                            // work is rejected at submission instead, where the
                            // pool recomputes the seed from the current tip.
                            state.tip = h;
                        }
                    }
                }
                state.height = work
                    .get("height")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1)
                    .saturating_sub(1);
                state.network_difficulty = work
                    .get("difficulty")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(u64::MAX);
                state.connected = true;
            }
            Err(err) => {
                let mut state = pool.state.lock();
                if state.connected {
                    tracing::warn!("lost the node: {err:#}");
                }
                state.connected = false;
            }
        }

        if let Ok(status) = rpc_call(&pool.config.node, "cog_status", json!({})).await {
            if let Some(id) = status.get("chain_id").and_then(|c| c.as_str()) {
                pool.state.lock().chain_id = id.to_string();
            }
        }

        // Pick up challenges the node is waiting on for our blocks.
        if let Ok(v) = rpc_call(
            &pool.config.node,
            "cog_getRevealRequests",
            json!({ "miner": pool.address.to_hex() }),
        )
        .await
        {
            if let Some(list) = v.get("requests").and_then(|r| r.as_array()) {
                let mut state = pool.state.lock();
                for req in list {
                    let id = match req.get("commit_id").and_then(|c| c.as_str()) {
                        Some(s) => match hex::decode(s) {
                            Ok(raw) if raw.len() == 32 => {
                                let mut h = [0u8; 32];
                                h.copy_from_slice(&raw);
                                h
                            }
                            _ => continue,
                        },
                        None => continue,
                    };
                    let rows: Vec<u32> = req
                        .get("rows")
                        .and_then(|r| r.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_u64())
                                .map(|v| v as u32)
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(finder) = state.found_by.get(&id).copied() {
                        state.node_requests.insert(id, (rows, finder));
                    }
                }
            }
        }

        // Drop shares whose owner never answered the spot-check.
        {
            let cutoff = now().saturating_sub(SHARE_TIMEOUT_SECS);
            let mut state = pool.state.lock();
            state.pending.retain(|_, s| s.created >= cutoff);
        }

        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}

/// Credit blocks the pool mined once the chain has settled their reward.
pub async fn account_rewards(pool: Pool) {
    loop {
        let reward = match rpc_call(&pool.config.node, "cog_getSupply", json!({})).await {
            Ok(v) => v
                .get("current_block_reward_cog")
                .and_then(|s| s.as_str())
                .and_then(|s| crate::types::parse_cog(s).ok())
                .unwrap_or(0),
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        {
            let mut state = pool.state.lock();
            state.block_reward = reward;
        }

        let (from, tip) = {
            let state = pool.state.lock();
            (state.ledger.last_scanned_height + 1, state.height)
        };

        // A block's reward only settles when the *next* block opens its
        // commitment, so never look at the tip itself.
        let mut height = from;
        while height < tip {
            let block = match rpc_call(
                &pool.config.node,
                "cog_getBlock",
                json!({ "height": height }),
            )
            .await
            {
                Ok(b) => b,
                Err(_) => break,
            };
            let next = match rpc_call(
                &pool.config.node,
                "cog_getBlock",
                json!({ "height": height + 1 }),
            )
            .await
            {
                Ok(b) => b,
                Err(_) => break,
            };

            let mined_by_us = block
                .get("solution")
                .and_then(|s| s.get("miner"))
                .and_then(|m| m.as_str())
                .map(|m| m == pool.address.to_hex())
                .unwrap_or(false);

            if mined_by_us {
                // Recompute this block's commitment id and see whether the next
                // block opened it. Only then was anything actually minted.
                let settled = (|| -> Option<bool> {
                    let sol = block.get("solution")?;
                    let prev = hex::decode(block.get("prev_hash")?.as_str()?).ok()?;
                    let root = hex::decode(sol.get("matmul_root")?.as_str()?).ok()?;
                    if prev.len() != 32 || root.len() != 32 {
                        return None;
                    }
                    let mut prev_hash = [0u8; 32];
                    prev_hash.copy_from_slice(&prev);
                    let mut matmul_root = [0u8; 32];
                    matmul_root.copy_from_slice(&root);
                    let salt = sol.get("salt")?.as_u64()?;
                    let nonce = sol.get("nonce")?.as_u64()?;
                    let seed = pouw::task_seed(&prev_hash, &pool.address, salt);
                    let id = hex::encode(pouw::commit_id(&seed, &matmul_root, nonce));
                    Some(next.get("reveal")?.get("commit_id")?.as_str()? == id)
                })()
                .unwrap_or(false);

                if settled {
                    let fees: u64 = block
                        .get("fee_total_acog")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let mut state = pool.state.lock();
                    let gross = reward.saturating_add(fees);
                    pool.credit_block(&mut state, gross, height);
                    let ledger = state.ledger.clone();
                    drop(state);
                    pool.save(&ledger);
                }
            }

            let mut state = pool.state.lock();
            state.ledger.last_scanned_height = height;
            height += 1;
            if height % 200 == 0 {
                let ledger = state.ledger.clone();
                drop(state);
                pool.save(&ledger);
            }
        }

        {
            let state = pool.state.lock();
            let ledger = state.ledger.clone();
            drop(state);
            pool.save(&ledger);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Pay out everyone above the minimum, one transaction at a time.
pub async fn pay_miners(pool: Pool, interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;

        let due: Vec<(Address, u64)> = {
            let state = pool.state.lock();
            state
                .ledger
                .credits
                .iter()
                .filter(|(_, c)| **c >= pool.config.min_payout)
                .map(|(a, c)| (*a, *c))
                .collect()
        };
        if due.is_empty() {
            continue;
        }

        // One nonce per transaction, assigned locally. Re-reading the node's
        // nonce for each payout would hand the same value to two transactions,
        // and only one of them could ever be mined - while both miners had
        // already had their credit cleared.
        let chain_id = pool.state.lock().chain_id.clone();
        if chain_id.is_empty() {
            tracing::warn!("chain id still unknown, postponing payouts");
            continue;
        }

        let mut nonce = match rpc_call(
            &pool.config.node,
            "cog_getBalance",
            json!({ "address": pool.address.to_hex() }),
        )
        .await
        {
            Ok(v) => v.get("nonce").and_then(|n| n.as_u64()).unwrap_or(0),
            Err(err) => {
                tracing::warn!("cannot read pool nonce, postponing payouts: {err:#}");
                continue;
            }
        };

        // The mempool only tolerates a limited forward nonce gap, so pay in
        // batches and let the rest wait for the next round.
        for (addr, credit) in due.into_iter().take(MAX_PAYOUTS_PER_ROUND) {
            // The payout transaction's own fee comes out of the miner's credit,
            // so the pool never subsidises nor skims it.
            let amount = match credit.checked_sub(pool.config.payout_fee) {
                Some(v) if v > 0 => v,
                _ => continue,
            };

            let tx = match pool.keypair.sign_transfer(
                &chain_id,
                addr,
                amount,
                pool.config.payout_fee,
                nonce,
                Vec::new(),
            ) {
                Ok(tx) => tx,
                Err(err) => {
                    tracing::error!("cannot sign payout to {addr}: {err:#}");
                    continue;
                }
            };

            match rpc_call(
                &pool.config.node,
                "cog_sendTransaction",
                json!({
                    "pubkey": hex::encode(tx.pubkey),
                    "to": tx.to.to_hex(),
                    "amount": tx.amount,
                    "fee": tx.fee,
                    "nonce": tx.nonce,
                    "memo": "",
                    "signature": hex::encode(tx.signature),
                }),
            )
            .await
            {
                Ok(_) => {
                    // Only clear the credit once the node has accepted the
                    // transaction. Clearing first would lose a miner's earnings
                    // whenever a send failed.
                    nonce += 1;
                    let mut state = pool.state.lock();
                    let entry = state.ledger.credits.entry(addr).or_insert(0);
                    *entry = entry.saturating_sub(credit);
                    if *entry == 0 {
                        state.ledger.credits.remove(&addr);
                    }
                    *state.ledger.paid.entry(addr).or_insert(0) += amount;
                    let ledger = state.ledger.clone();
                    drop(state);
                    pool.save(&ledger);
                    tracing::info!("paid {} COG to {addr}", format_cog(amount));
                }
                Err(err) => {
                    tracing::warn!("payout to {addr} failed, keeping the credit: {err:#}");
                }
            }
        }
    }
}
