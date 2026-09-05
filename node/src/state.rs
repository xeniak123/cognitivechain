//! The replicated state machine: accounts, supply accounting and pending
//! useful-work commitments.

use crate::crypto::verify_transaction;
use crate::genesis::{block_reward, GenesisConfig};
use crate::pouw::{self, REVEAL_WINDOW};
use crate::types::{Address, Block, Hash, Transaction, MAX_MEMO_LEN};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    pub balance: u64,
    /// Next expected transaction nonce for this account.
    pub nonce: u64,
}

/// A commitment awaiting its reveal. The reward is fixed at commit time so the
/// value does not depend on when the reveal lands.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingCommit {
    pub height: u64,
    pub miner: Address,
    pub task_seed: Hash,
    pub matmul_root: Hash,
    pub reward: u64,
    /// Last height at which the reveal is still accepted.
    pub expires_at: u64,
}

/// Chain parameters resolved from genesis into plain integers.
#[derive(Clone, Debug)]
pub struct ChainParams {
    pub chain_id: String,
    pub max_supply: u64,
    pub initial_reward: u64,
    pub halving_interval_tasks: u64,
    pub target_block_time_secs: u64,
    pub retarget_interval: u64,
    pub initial_difficulty: u64,
    pub min_tx_fee: u64,
    pub max_block_txs: usize,
    pub max_future_drift_secs: u64,
}

impl ChainParams {
    pub fn from_genesis(cfg: &GenesisConfig) -> Result<Self> {
        Ok(ChainParams {
            chain_id: cfg.chain_id.clone(),
            max_supply: cfg.max_supply()?,
            initial_reward: cfg.initial_reward()?,
            halving_interval_tasks: cfg.params.halving_interval_tasks,
            target_block_time_secs: cfg.params.target_block_time_secs,
            retarget_interval: cfg.params.retarget_interval,
            initial_difficulty: cfg.params.initial_difficulty,
            min_tx_fee: cfg.min_tx_fee()?,
            max_block_txs: cfg.params.max_block_txs,
            max_future_drift_secs: cfg.params.max_future_drift_secs,
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    /// Hard supply cap, copied from genesis. Fixed for the life of the chain and
    /// therefore deliberately excluded from the state root.
    pub supply_cap: u64,
    pub accounts: BTreeMap<Address, Account>,
    /// Total acog ever created (premine + mined rewards). Never exceeds max supply.
    pub minted: u64,
    /// Number of useful-work tasks that have been revealed and verified.
    pub tasks_completed: u64,
    /// Commitments that still owe a reveal, keyed by `pouw::commit_id`.
    pub pending: BTreeMap<Hash, PendingCommit>,
}

impl State {
    pub fn balance(&self, addr: &Address) -> u64 {
        self.accounts.get(addr).map(|a| a.balance).unwrap_or(0)
    }

    pub fn nonce(&self, addr: &Address) -> u64 {
        self.accounts.get(addr).map(|a| a.nonce).unwrap_or(0)
    }

    fn credit(&mut self, addr: &Address, amount: u64) -> Result<()> {
        let acct = self.accounts.entry(*addr).or_default();
        acct.balance = match acct.balance.checked_add(amount) {
            Some(v) => v,
            None => bail!("balance overflow crediting {addr}"),
        };
        Ok(())
    }

    fn debit(&mut self, addr: &Address, amount: u64) -> Result<()> {
        let acct = self.accounts.entry(*addr).or_default();
        if acct.balance < amount {
            bail!(
                "insufficient balance for {addr}: has {}, needs {amount}",
                acct.balance
            );
        }
        acct.balance -= amount;
        Ok(())
    }

    fn mint(&mut self, addr: &Address, amount: u64) -> Result<u64> {
        if amount == 0 {
            return Ok(0);
        }
        let remaining = self.max_remaining_supply();
        let actual = amount.min(remaining);
        if actual == 0 {
            return Ok(0);
        }
        self.credit(addr, actual)?;
        self.minted += actual;
        Ok(actual)
    }

    fn max_remaining_supply(&self) -> u64 {
        self.supply_cap.saturating_sub(self.minted)
    }

    /// Commitment to the whole state, bound into every block header.
    pub fn root(&self) -> Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"cog/state/v1");
        hasher.update(&(self.accounts.len() as u64).to_le_bytes());
        for (addr, acct) in &self.accounts {
            hasher.update(&addr.0);
            hasher.update(&acct.balance.to_le_bytes());
            hasher.update(&acct.nonce.to_le_bytes());
        }
        hasher.update(&self.minted.to_le_bytes());
        hasher.update(&self.tasks_completed.to_le_bytes());
        hasher.update(&(self.pending.len() as u64).to_le_bytes());
        for (hash, p) in &self.pending {
            hasher.update(hash);
            hasher.update(&p.height.to_le_bytes());
            hasher.update(&p.miner.0);
            hasher.update(&p.task_seed);
            hasher.update(&p.matmul_root);
            hasher.update(&p.reward.to_le_bytes());
            hasher.update(&p.expires_at.to_le_bytes());
        }
        *hasher.finalize().as_bytes()
    }
}

impl State {
    pub fn with_cap(mut self, cap: u64) -> Self {
        self.supply_cap = cap;
        self
    }
}

/// Validate and apply a transaction. Fees are collected into `fees_collected`.
fn apply_transaction(
    state: &mut State,
    tx: &Transaction,
    params: &ChainParams,
    fees_collected: &mut u64,
) -> Result<()> {
    if tx.chain_id != params.chain_id {
        bail!(
            "transaction is for chain {:?}, this is {:?}",
            tx.chain_id,
            params.chain_id
        );
    }
    if tx.memo.len() > MAX_MEMO_LEN {
        bail!("memo too long: {} > {MAX_MEMO_LEN}", tx.memo.len());
    }
    if tx.fee < params.min_tx_fee {
        bail!("fee {} below minimum {}", tx.fee, params.min_tx_fee);
    }
    verify_transaction(tx)?;

    let from = tx.from();
    if from == tx.to {
        bail!("self-transfer is not allowed");
    }
    let expected_nonce = state.nonce(&from);
    if tx.nonce != expected_nonce {
        bail!(
            "bad nonce for {from}: got {}, expected {expected_nonce}",
            tx.nonce
        );
    }
    let total = tx
        .amount
        .checked_add(tx.fee)
        .ok_or_else(|| anyhow::anyhow!("amount + fee overflows u64"))?;

    state.debit(&from, total)?;
    state.credit(&tx.to, tx.amount)?;
    state.accounts.entry(from).or_default().nonce = expected_nonce + 1;
    *fees_collected = fees_collected
        .checked_add(tx.fee)
        .ok_or_else(|| anyhow::anyhow!("fee accumulator overflow"))?;
    Ok(())
}

/// Verify a reveal against the pending commitment it opens.
///
/// This is the only place where useful work is checked, and it costs
/// `CHALLENGE_ROWS * O(N^2)` instead of the `O(N^3)` the miner had to spend.
///
/// The challenge seed is `block.header.prev_hash`, i.e. the hash of the block
/// that carried the commitment. That value did not exist when the miner chose
/// its commitment, which is what makes selective computation unprofitable.
pub fn verify_reveal(state: &State, block: &Block) -> Result<PendingCommit> {
    let reveal = match &block.reveal {
        Some(r) => r,
        None => bail!("block carries no reveal"),
    };
    let pending = match state.pending.get(&reveal.commit_id) {
        Some(p) => p.clone(),
        None => bail!(
            "reveal references unknown commitment {}",
            hex::encode(reveal.commit_id)
        ),
    };
    if pending.height + REVEAL_WINDOW != block.header.height {
        bail!(
            "commitment from height {} must be opened at height {}, not {}",
            pending.height,
            pending.height + REVEAL_WINDOW,
            block.header.height
        );
    }

    let expected_rows = pouw::challenge_rows(&block.header.prev_hash);
    if reveal.rows.len() != expected_rows.len() {
        bail!(
            "reveal must open {} rows, got {}",
            expected_rows.len(),
            reveal.rows.len()
        );
    }

    let a = pouw::gen_matrix_a(&pending.task_seed);
    let b = pouw::gen_matrix_b(&pending.task_seed);

    for (slot, row_proof) in reveal.rows.iter().enumerate() {
        let want_index = expected_rows[slot];
        if row_proof.index != want_index {
            bail!(
                "reveal row {slot} opens index {} but the challenge demands {want_index}",
                row_proof.index
            );
        }
        if row_proof.values.len() != pouw::N {
            bail!(
                "reveal row {slot} has {} values, expected {}",
                row_proof.values.len(),
                pouw::N
            );
        }
        let idx = row_proof.index as usize;
        let leaf = pouw::leaf_hash(row_proof.index, &row_proof.values);
        if !pouw::merkle_verify(
            &pending.matmul_root,
            row_proof.index,
            &leaf,
            &row_proof.proof,
        ) {
            bail!("reveal row {slot} (index {idx}) fails its Merkle inclusion proof");
        }
        let recomputed = pouw::matmul_row(&a[idx * pouw::N..idx * pouw::N + pouw::N], &b);
        if recomputed != row_proof.values {
            bail!("reveal row {slot} (index {idx}) does not match the recomputed product");
        }
    }
    Ok(pending)
}

/// Apply a full block to the state. Assumes structural/PoW validation already
/// happened in `chain::validate_block`; this function still enforces every
/// economic rule so that it can never be bypassed.
pub fn apply_block(state: &mut State, block: &Block, params: &ChainParams) -> Result<()> {
    if block.transactions.len() > params.max_block_txs {
        bail!(
            "block carries {} transactions, limit is {}",
            block.transactions.len(),
            params.max_block_txs
        );
    }

    let mut fees_collected = 0u64;
    for tx in &block.transactions {
        apply_transaction(state, tx, params, &mut fees_collected)?;
    }

    // 1. Settle a reveal, if present: this is what actually mints coins.
    if let Some(reveal) = &block.reveal {
        let pending = verify_reveal(state, block)?;
        state.pending.remove(&reveal.commit_id);
        state.mint(&pending.miner, pending.reward)?;
        state.tasks_completed += 1;
    }

    // 2. Drop commitments whose reveal window has closed. Their reward is simply
    //    never minted, which is the whole penalty for an unopened commitment.
    let height = block.header.height;
    state.pending.retain(|_, p| p.expires_at > height);

    // 3. Register this block's own commitment and pay out its transaction fees.
    if let Some(sol) = &block.solution {
        let task_seed = pouw::task_seed(&block.header.prev_hash, &sol.miner, sol.salt);
        let id = pouw::commit_id(&task_seed, &sol.matmul_root, sol.nonce);
        let reward = block_reward(
            params.initial_reward,
            params.halving_interval_tasks,
            state.tasks_completed,
        );
        state.pending.insert(
            id,
            PendingCommit {
                height,
                miner: sol.miner,
                task_seed,
                matmul_root: sol.matmul_root,
                reward,
                expires_at: height + REVEAL_WINDOW,
            },
        );
        // Fees are paid immediately and unconditionally to the block producer;
        // they are a transfer, not an issuance, so they do not touch `minted`.
        if fees_collected > 0 {
            state.credit(&sol.miner, fees_collected)?;
        }
    } else if fees_collected > 0 {
        bail!("a block without a solution cannot collect fees");
    }

    if state.minted > state.supply_cap {
        bail!(
            "invariant violated: minted {} exceeds cap {}",
            state.minted,
            state.supply_cap
        );
    }
    Ok(())
}

/// Build the initial state from the genesis allocations.
pub fn genesis_state(cfg: &GenesisConfig) -> Result<State> {
    cfg.validate()?;
    cfg.assert_unique_allocations()?;
    let mut state = State::default().with_cap(cfg.max_supply()?);
    for alloc in &cfg.allocations {
        let amount: u64 = alloc.amount_acog.parse()?;
        state.credit(&alloc.address, amount)?;
        state.minted += amount;
    }
    if state.minted > state.supply_cap {
        bail!("genesis allocations exceed max supply");
    }
    Ok(state)
}
