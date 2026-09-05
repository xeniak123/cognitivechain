//! JSON-RPC 2.0 endpoint used by miners, wallets and explorers.
//!
//! A single `POST /` accepts `{"jsonrpc":"2.0","id":..,"method":..,"params":{..}}`.
//! `GET /health` is a plain liveness probe for load balancers and Docker.

use crate::chain::{Accepted, Chain};
use crate::pouw;
use crate::types::{format_cog, Address, Block, Hash, Reveal, RowProof, Solution, Transaction};
use anyhow::{anyhow, bail, Context, Result};
use axum::extract::State as AxumState;
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct RpcContext {
    pub chain: Arc<Mutex<Chain>>,
    /// Blocks accepted locally are published here so the P2P task can gossip them.
    pub announce: broadcast::Sender<Block>,
}

pub async fn serve(ctx: RpcContext, addr: SocketAddr) -> Result<()> {
    let app = Router::new()
        .route("/", post(handle_rpc))
        .route("/health", get(|| async { "ok" }))
        .with_state(ctx);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind RPC listener on {addr}"))?;
    tracing::info!("JSON-RPC listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_rpc(AxumState(ctx): AxumState<RpcContext>, Json(req): Json<Value>) -> Json<Value> {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let params = req.get("params").cloned().unwrap_or(json!({}));

    let result = tokio::task::block_in_place(|| dispatch(&ctx, &method, &params));
    match result {
        Ok(value) => Json(json!({"jsonrpc": "2.0", "id": id, "result": value})),
        Err(err) => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32000, "message": format!("{err:#}")}
        })),
    }
}

fn str_param(params: &Value, key: &str) -> Result<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing string parameter `{key}`"))
}

fn u64_param(params: &Value, key: &str) -> Result<u64> {
    match params.get(key) {
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| anyhow!("parameter `{key}` must be a non-negative integer")),
        Some(Value::String(s)) => s
            .parse::<u64>()
            .map_err(|_| anyhow!("parameter `{key}` must be an integer, got {s:?}")),
        _ => Err(anyhow!("missing integer parameter `{key}`")),
    }
}

fn hash_param(params: &Value, key: &str) -> Result<Hash> {
    let raw = hex::decode(str_param(params, key)?)?;
    if raw.len() != 32 {
        bail!("`{key}` must be a 32-byte hex string");
    }
    let mut h = [0u8; 32];
    h.copy_from_slice(&raw);
    Ok(h)
}

fn addr_param(params: &Value, key: &str) -> Result<Address> {
    Address::parse(&str_param(params, key)?).map_err(|e| anyhow!("`{key}`: {e}"))
}

fn block_to_json(block: &Block) -> Value {
    json!({
        "hash": hex::encode(block.hash()),
        "height": block.header.height,
        "prev_hash": hex::encode(block.header.prev_hash),
        "timestamp": block.header.timestamp,
        "difficulty": block.header.difficulty,
        "tx_root": hex::encode(block.header.tx_root),
        "state_root": hex::encode(block.header.state_root),
        "reveal_root": hex::encode(block.header.reveal_root),
        "transaction_count": block.transactions.len(),
        "solution": block.solution.as_ref().map(|s| json!({
            "miner": s.miner.to_hex(),
            "salt": s.salt,
            "nonce": s.nonce,
            "matmul_root": hex::encode(s.matmul_root),
        })),
        "reveal": block.reveal.as_ref().map(|r| json!({
            "commit_id": hex::encode(r.commit_id),
            "rows": r.rows.iter().map(|row| row.index).collect::<Vec<_>>(),
        })),
    })
}

fn dispatch(ctx: &RpcContext, method: &str, params: &Value) -> Result<Value> {
    match method {
        "cog_status" => {
            let chain = ctx.chain.lock();
            Ok(json!({
                "chain_id": chain.params.chain_id,
                "height": chain.tip.header.height,
                "tip_hash": hex::encode(chain.tip_hash),
                "difficulty": chain.tip.header.difficulty,
                "cumulative_work": chain.tip_work.to_string(),
                "tasks_completed": chain.state.tasks_completed,
                "minted_acog": chain.state.minted.to_string(),
                "minted_cog": format_cog(chain.state.minted),
                "max_supply_acog": chain.state.supply_cap.to_string(),
                "max_supply_cog": format_cog(chain.state.supply_cap),
                "mempool_size": chain.mempool.len(),
                "pending_commitments": chain.state.pending.len(),
                "accounts": chain.state.accounts.len(),
            }))
        }

        // Everything a miner needs to build its next task.
        "cog_getWork" => {
            let chain = ctx.chain.lock();
            let difficulty = chain.expected_difficulty(&chain.tip)?;
            Ok(json!({
                "chain_id": chain.params.chain_id,
                "height": chain.tip.header.height + 1,
                "prev_hash": hex::encode(chain.tip_hash),
                "difficulty": difficulty,
                "matrix_dim": pouw::N,
                "field_prime": pouw::P,
                "max_nonce": pouw::MAX_NONCE,
                "challenge_rows": pouw::CHALLENGE_ROWS,
                "target_block_time_secs": chain.params.target_block_time_secs,
            }))
        }

        "cog_submitSolution" => {
            let sol = Solution {
                miner: addr_param(params, "miner")?,
                salt: u64_param(params, "salt")?,
                nonce: u64_param(params, "nonce")?,
                matmul_root: hash_param(params, "matmul_root")?,
            };
            let seed = {
                let chain = ctx.chain.lock();
                pouw::task_seed(&chain.tip_hash, &sol.miner, sol.salt)
            };
            let commit_id = pouw::commit_id(&seed, &sol.matmul_root, sol.nonce);

            let (outcome, block) = {
                let mut chain = ctx.chain.lock();
                let outcome = chain.submit_solution(sol)?;
                let block = chain.tip.clone();
                (outcome, block)
            };
            match outcome {
                Accepted::Extended { hash, height } => {
                    let _ = ctx.announce.send(block);
                    Ok(json!({
                        "status": "accepted",
                        "block_hash": hex::encode(hash),
                        "height": height,
                        "commit_id": hex::encode(commit_id),
                        "message": "commitment stored; submit the reveal for the next block",
                    }))
                }
                other => Ok(json!({
                    "status": "rejected",
                    "detail": format!("{other:?}"),
                })),
            }
        }

        // Which commitments of this miner still owe an opening, and which rows.
        "cog_getRevealRequests" => {
            let miner = addr_param(params, "miner")?;
            let chain = ctx.chain.lock();
            let requests = chain.reveal_requests(&miner);
            Ok(json!({
                "requests": requests
                    .into_iter()
                    .map(|(id, seed, rows)| json!({
                        "commit_id": hex::encode(id),
                        "task_seed": hex::encode(seed),
                        "rows": rows,
                    }))
                    .collect::<Vec<_>>()
            }))
        }

        "cog_submitReveal" => {
            let commit_id = hash_param(params, "commit_id")?;
            let rows_json = params
                .get("rows")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("missing array parameter `rows`"))?;

            let mut rows = Vec::with_capacity(rows_json.len());
            for entry in rows_json {
                let index = u64_param(entry, "index")? as u32;
                let values_hex = str_param(entry, "values")?;
                let raw = hex::decode(values_hex)?;
                if raw.len() != pouw::N * 2 {
                    bail!("row {index}: `values` must be {} hex bytes", pouw::N * 2);
                }
                let values: Vec<u16> = raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let proof_json = entry
                    .get("proof")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow!("row {index}: missing `proof`"))?;
                let mut proof = Vec::with_capacity(proof_json.len());
                for p in proof_json {
                    let bytes = hex::decode(p.as_str().ok_or_else(|| {
                        anyhow!("row {index}: proof entries must be hex strings")
                    })?)?;
                    if bytes.len() != 32 {
                        bail!("row {index}: proof entries must be 32 bytes");
                    }
                    let mut h = [0u8; 32];
                    h.copy_from_slice(&bytes);
                    proof.push(h);
                }
                rows.push(RowProof {
                    index,
                    values,
                    proof,
                });
            }

            let mut chain = ctx.chain.lock();
            chain.submit_reveal(Reveal { commit_id, rows })?;
            Ok(json!({"status": "accepted"}))
        }

        "cog_getBalance" => {
            let address = addr_param(params, "address")?;
            let chain = ctx.chain.lock();
            let balance = chain.state.balance(&address);
            Ok(json!({
                "address": address.to_hex(),
                "balance_acog": balance.to_string(),
                "balance_cog": format_cog(balance),
                "nonce": chain.state.nonce(&address),
            }))
        }

        "cog_sendTransaction" => {
            let pubkey_raw = hex::decode(str_param(params, "pubkey")?)?;
            if pubkey_raw.len() != 32 {
                bail!("`pubkey` must be 32 bytes");
            }
            let mut pubkey = [0u8; 32];
            pubkey.copy_from_slice(&pubkey_raw);

            let sig_raw = hex::decode(str_param(params, "signature")?)?;
            if sig_raw.len() != 64 {
                bail!("`signature` must be 64 bytes");
            }
            let mut signature = [0u8; 64];
            signature.copy_from_slice(&sig_raw);

            let memo = match params.get("memo") {
                Some(Value::String(s)) => hex::decode(s)?,
                _ => Vec::new(),
            };

            let tx = Transaction {
                pubkey,
                to: addr_param(params, "to")?,
                amount: u64_param(params, "amount")?,
                fee: u64_param(params, "fee")?,
                nonce: u64_param(params, "nonce")?,
                memo,
                signature,
            };
            let mut chain = ctx.chain.lock();
            let state = chain.state.clone();
            let chain_params = chain.params.clone();
            let hash = chain.mempool.insert(tx, &state, &chain_params)?;
            Ok(json!({"status": "queued", "tx_hash": hex::encode(hash)}))
        }

        "cog_getBlock" => {
            let chain = ctx.chain.lock();
            let block = if params.get("hash").is_some() {
                chain.get_block(&hash_param(params, "hash")?)?
            } else {
                chain.block_at_height(u64_param(params, "height")?)?
            };
            match block {
                Some(b) => Ok(block_to_json(&b)),
                None => Err(anyhow!("block not found")),
            }
        }

        "cog_getSupply" => {
            let chain = ctx.chain.lock();
            let remaining = chain.state.supply_cap.saturating_sub(chain.state.minted);
            Ok(json!({
                "minted_acog": chain.state.minted.to_string(),
                "minted_cog": format_cog(chain.state.minted),
                "remaining_acog": remaining.to_string(),
                "remaining_cog": format_cog(remaining),
                "max_supply_cog": format_cog(chain.state.supply_cap),
                "tasks_completed": chain.state.tasks_completed,
                "current_block_reward_cog": format_cog(crate::genesis::block_reward(
                    chain.params.initial_reward,
                    chain.params.halving_interval_tasks,
                    chain.state.tasks_completed,
                )),
            }))
        }

        other => Err(anyhow!("unknown method `{other}`")),
    }
}
