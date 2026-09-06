//! Block validation, chain selection and block production.

use crate::genesis::GenesisConfig;
use crate::mempool::Mempool;
use crate::pouw;
use crate::state::{apply_block, genesis_state, ChainParams, State};
use crate::store::Store;
use crate::types::{
    meets_difficulty, reveal_root, tx_root, Block, BlockHeader, Hash, Reveal, Solution, Transaction,
};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// How often the post-state of a block is written to disk. A restart replays at
/// most this many blocks, and a reorganisation rewinds to at most this depth
/// before it finds a state it can start from.
const SNAPSHOT_INTERVAL: u64 = 100;
/// Snapshots older than this many blocks behind the tip are pruned.
const SNAPSHOT_RETENTION: u64 = 1_000;

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Outcome of offering a block to the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    /// The block extended the canonical tip.
    Extended { hash: Hash, height: u64 },
    /// The block was already known.
    Duplicate,
    /// Stored on a side branch that does not (yet) beat the canonical chain.
    SideBranch { hash: Hash, height: u64 },
    /// The parent is unknown; the caller should request the missing ancestors.
    Orphan { missing_parent: Hash },
    /// A heavier branch was adopted and the state was rebuilt along it.
    Reorganised { hash: Hash, height: u64, depth: u64 },
}

pub struct Chain {
    store: Store,
    pub params: ChainParams,
    pub genesis_cfg: GenesisConfig,
    pub state: State,
    pub tip: Block,
    pub tip_hash: Hash,
    pub tip_work: u128,
    pub mempool: Mempool,
    /// Reveals received from miners that have not been included in a block yet,
    /// keyed by `commit_id`.
    pub reveal_buffer: HashMap<Hash, Reveal>,
}

impl Chain {
    /// Open an existing database or initialise a fresh one from `cfg`.
    pub fn open(data_dir: &Path, cfg: GenesisConfig) -> Result<Self> {
        let store = Store::open(&data_dir.join("db"))?;
        let params = ChainParams::from_genesis(&cfg)?;

        if let Some(existing) = store.chain_id()? {
            if existing != cfg.chain_id {
                bail!(
                    "data directory belongs to chain {existing:?} but genesis says {:?}; \
                     use a different --data-dir",
                    cfg.chain_id
                );
            }
        } else {
            store.set_chain_id(&cfg.chain_id)?;
        }

        let genesis_state = genesis_state(&cfg)?;
        let genesis_block = cfg.genesis_block(genesis_state.root());
        let genesis_hash = genesis_block.hash();

        if let Some(stored) = store.genesis()? {
            if stored != genesis_hash {
                bail!(
                    "stored genesis {} does not match the supplied genesis file ({}); \
                     the chain parameters or allocations were changed",
                    hex::encode(stored),
                    hex::encode(genesis_hash)
                );
            }
        } else {
            store.put_block(&genesis_block, 0)?;
            store.set_canonical(0, &genesis_hash)?;
            store.set_tip(&genesis_hash)?;
            store.set_genesis(&genesis_hash)?;
            store.flush()?;
        }

        // A genesis timestamp in the future is a legitimate way to schedule a
        // launch: block 1 must carry a timestamp greater than genesis and no
        // greater than now, so no block can exist until that moment arrives.
        // It is also an easy thing to leave in a template by accident, so say so.
        let now = now_secs();
        if cfg.genesis_time > now {
            let wait = cfg.genesis_time - now;
            tracing::warn!(
                "genesis_time ({}) is {} days in the future: this chain accepts no blocks                  until then. If that is not intentional, set genesis_time in the genesis                  file to the real launch time.",
                cfg.genesis_time,
                wait / 86_400
            );
        }

        let tip_hash = store.tip()?.unwrap_or(genesis_hash);
        let mut chain = Chain {
            store,
            params,
            genesis_cfg: cfg,
            state: genesis_state,
            tip: genesis_block,
            tip_hash: genesis_hash,
            tip_work: 0,
            mempool: Mempool::new(),
            reveal_buffer: HashMap::new(),
        };
        chain.rebuild_state_to(&tip_hash)?;
        Ok(chain)
    }

    pub fn genesis_hash(&self) -> Result<Hash> {
        self.store
            .genesis()?
            .context("genesis hash missing from store")
    }

    pub fn get_block(&self, hash: &Hash) -> Result<Option<Block>> {
        self.store.get_block(hash)
    }

    pub fn block_at_height(&self, height: u64) -> Result<Option<Block>> {
        match self.store.canonical_at(height)? {
            Some(h) => self.store.get_block(&h),
            None => Ok(None),
        }
    }

    /// Walk back from `hash` to genesis, returning the branch in forward order.
    fn branch_to(&self, hash: &Hash) -> Result<Vec<Block>> {
        let genesis_hash = self.genesis_hash()?;
        let mut chain = Vec::new();
        let mut cursor = *hash;
        loop {
            let block = self.store.get_block(&cursor)?.with_context(|| {
                format!("missing block {} while walking back", hex::encode(cursor))
            })?;
            let prev = block.header.prev_hash;
            let is_genesis = cursor == genesis_hash;
            chain.push(block);
            if is_genesis {
                break;
            }
            cursor = prev;
        }
        chain.reverse();
        Ok(chain)
    }

    /// Adopt `hash` as the tip, rebuilding the state to match it.
    ///
    /// Walks back only as far as the nearest block whose post-state was saved,
    /// then replays forward from there. Without this a node re-executed - and
    /// re-verified every proof in - the entire chain on every single start,
    /// which is seconds at a few thousand blocks and minutes at a few hundred
    /// thousand. It is also what keeps a reorganisation from costing a full
    /// replay from genesis.
    fn rebuild_state_to(&mut self, hash: &Hash) -> Result<()> {
        let genesis_hash = self.genesis_hash()?;

        // Collect blocks back to the nearest usable state, newest first.
        let mut suffix: Vec<Block> = Vec::new();
        let mut cursor = *hash;
        let mut base: Option<State> = None;

        loop {
            if let Some(raw) = self.store.get_state(&cursor)? {
                match bincode::deserialize::<State>(&raw) {
                    Ok(state) => {
                        base = Some(state);
                        break;
                    }
                    // A snapshot written by an older build is not a reason to
                    // fail; fall through and replay instead.
                    Err(err) => {
                        tracing::warn!("ignoring an unreadable state snapshot: {err}");
                    }
                }
            }
            let block = self.store.get_block(&cursor)?.with_context(|| {
                format!("missing block {} while walking back", hex::encode(cursor))
            })?;
            let prev = block.header.prev_hash;
            let is_genesis = cursor == genesis_hash;
            suffix.push(block);
            if is_genesis {
                break;
            }
            cursor = prev;
        }

        let mut state = match base {
            Some(state) => state,
            None => genesis_state(&self.genesis_cfg)?,
        };

        suffix.reverse();
        if !suffix.is_empty() {
            tracing::info!("replaying {} blocks to reach the tip", suffix.len());
        }
        for block in &suffix {
            // The genesis block carries no solution and no transactions; its
            // post-state is what `genesis_state` already produced.
            if block.header.height == 0 {
                continue;
            }
            apply_block(&mut state, block, &self.params)?;
        }

        let tip = self
            .store
            .get_block(hash)?
            .context("tip block vanished from the store")?;

        // The canonical index is rewritten only for what was replayed; anything
        // above the new tip belonged to a branch we just left.
        for block in &suffix {
            self.store
                .set_canonical(block.header.height, &block.hash())?;
        }
        self.store.truncate_canonical_above(tip.header.height)?;
        self.store.set_tip(hash)?;

        self.tip_work = self.store.cumulative_work(hash)?.unwrap_or(0);
        self.tip_hash = *hash;
        self.tip = tip;
        self.state = state;
        self.save_snapshot()?;
        self.store.flush()?;
        Ok(())
    }

    /// Persist the current tip's post-state.
    fn save_snapshot(&self) -> Result<()> {
        let encoded = bincode::serialize(&self.state)?;
        self.store
            .put_state(&self.tip_hash, self.tip.header.height, &encoded)?;
        self.store
            .prune_states(self.tip.header.height.saturating_sub(SNAPSHOT_RETENTION))?;
        Ok(())
    }

    /// Difficulty that a child of `parent` must carry.
    pub fn expected_difficulty(&self, parent: &Block) -> Result<u64> {
        let height = parent.header.height + 1;
        let interval = self.params.retarget_interval;
        if !height.is_multiple_of(interval) || height < interval {
            return Ok(parent.header.difficulty);
        }

        // The window is the `interval` blocks at heights [height-interval, height-1],
        // so the first of them sits `interval - 1` steps back from the parent.
        // Walking `interval` steps instead would run off the end of the chain at
        // the very first retarget and halt the network there.
        let steps = interval - 1;
        let mut cursor = parent.clone();
        for _ in 0..steps {
            cursor = match self.store.get_block(&cursor.header.prev_hash)? {
                Some(block) => block,
                // An incomplete window (a pruned or still-syncing branch) is not
                // a reason to stop producing blocks; keep the parent difficulty.
                None => return Ok(parent.header.difficulty),
            };
        }
        let first = cursor;

        let actual = parent
            .header
            .timestamp
            .saturating_sub(first.header.timestamp)
            .max(1);
        // `steps` gaps were measured, so compare against `steps` target intervals.
        let expected = steps * self.params.target_block_time_secs;

        // difficulty scales inversely with elapsed time, clamped to 4x per window.
        let old = parent.header.difficulty as u128;
        let mut next = old * expected as u128 / actual as u128;
        let max = old.saturating_mul(4);
        let min = (old / 4).max(1);
        if next > max {
            next = max;
        }
        if next < min {
            next = min;
        }
        if next > u64::MAX as u128 {
            next = u64::MAX as u128;
        }
        Ok((next as u64).max(1))
    }

    /// Full stateless + stateful validation of `block` against `parent`.
    /// Returns the post-state on success.
    pub fn validate_block(
        &self,
        parent: &Block,
        parent_state: &State,
        block: &Block,
    ) -> Result<State> {
        if block.header.version != 1 {
            bail!("unsupported block version {}", block.header.version);
        }
        if block.header.height != parent.header.height + 1 {
            bail!(
                "height {} does not follow parent height {}",
                block.header.height,
                parent.header.height
            );
        }
        if block.header.prev_hash != parent.hash() {
            bail!("prev_hash does not match the parent block");
        }
        if block.header.timestamp <= parent.header.timestamp {
            bail!(
                "timestamp {} must be strictly greater than the parent's {}",
                block.header.timestamp,
                parent.header.timestamp
            );
        }
        let limit = now_secs() + self.params.max_future_drift_secs;
        if block.header.timestamp > limit {
            bail!(
                "timestamp {} is too far in the future",
                block.header.timestamp
            );
        }

        let want_difficulty = self.expected_difficulty(parent)?;
        if block.header.difficulty != want_difficulty {
            bail!(
                "difficulty {} does not match the expected {want_difficulty}",
                block.header.difficulty
            );
        }
        if block.header.tx_root != tx_root(&block.transactions) {
            bail!("tx_root does not commit to the transaction list");
        }
        if block.header.reveal_root != reveal_root(&block.reveal) {
            bail!("reveal_root does not commit to the reveal payload");
        }

        let sol = match &block.solution {
            Some(s) => s,
            None => bail!("only the genesis block may omit a solution"),
        };
        if sol.nonce >= pouw::MAX_NONCE {
            bail!(
                "nonce {} is outside the permitted range [0, {})",
                sol.nonce,
                pouw::MAX_NONCE
            );
        }
        let seed = pouw::task_seed(&block.header.prev_hash, &sol.miner, sol.salt);
        let pow = pouw::pow_hash(&seed, &sol.matmul_root, sol.nonce);
        if !meets_difficulty(&pow, block.header.difficulty) {
            bail!(
                "proof-of-useful-work hash {} does not satisfy difficulty {}",
                hex::encode(pow),
                block.header.difficulty
            );
        }

        let mut state = parent_state.clone();
        apply_block(&mut state, block, &self.params)?;
        if state.root() != block.header.state_root {
            bail!(
                "state_root mismatch: header says {}, execution produced {}",
                hex::encode(block.header.state_root),
                hex::encode(state.root())
            );
        }
        Ok(state)
    }

    /// Offer a block to the chain from any source (local miner or a peer).
    pub fn accept_block(&mut self, block: Block) -> Result<Accepted> {
        let hash = block.hash();
        if self.store.has_block(&hash)? {
            return Ok(Accepted::Duplicate);
        }
        let parent_hash = block.header.prev_hash;
        let parent = match self.store.get_block(&parent_hash)? {
            Some(p) => p,
            None => {
                return Ok(Accepted::Orphan {
                    missing_parent: parent_hash,
                })
            }
        };

        if parent_hash == self.tip_hash {
            // Fast path: the block extends what we already have.
            let new_state = self.validate_block(&parent, &self.state, &block)?;
            let work = self.tip_work + block.header.difficulty as u128;
            self.store.put_block(&block, work)?;
            self.store.set_canonical(block.header.height, &hash)?;
            self.store.set_tip(&hash)?;
            self.store.flush()?;

            self.mempool.prune(&block.transactions, &new_state);
            if let Some(r) = &block.reveal {
                self.reveal_buffer.remove(&r.commit_id);
            }
            self.reveal_buffer
                .retain(|_, r| new_state.pending.contains_key(&r.commit_id));

            self.state = new_state;
            self.tip = block;
            self.tip_hash = hash;
            self.tip_work = work;
            if self.tip.header.height.is_multiple_of(SNAPSHOT_INTERVAL) {
                self.save_snapshot()?;
            }
            return Ok(Accepted::Extended {
                hash,
                height: self.tip.header.height,
            });
        }

        // Side branch: replay it from genesis so we can validate and weigh it.
        let parent_work = self
            .store
            .cumulative_work(&parent_hash)?
            .context("parent block has no recorded work")?;
        let candidate_work = parent_work + block.header.difficulty as u128;

        let mut state = genesis_state(&self.genesis_cfg)?;
        let branch = self.branch_to(&parent_hash)?;
        for b in branch.iter().skip(1) {
            apply_block(&mut state, b, &self.params)?;
        }
        self.validate_block(&parent, &state, &block)?;
        self.store.put_block(&block, candidate_work)?;
        self.store.flush()?;

        if candidate_work <= self.tip_work {
            return Ok(Accepted::SideBranch {
                hash,
                height: block.header.height,
            });
        }

        let old_height = self.tip.header.height;
        self.rebuild_state_to(&hash)?;
        let state_snapshot = self.state.clone();
        self.mempool.prune(&[], &state_snapshot);
        self.reveal_buffer
            .retain(|_, r| state_snapshot.pending.contains_key(&r.commit_id));
        Ok(Accepted::Reorganised {
            hash,
            height: self.tip.header.height,
            depth: old_height.saturating_sub(block.header.height.saturating_sub(1)),
        })
    }

    /// Assemble a block on top of the current tip for a validated solution.
    pub fn build_block(&self, sol: Solution, transactions: Vec<Transaction>) -> Result<Block> {
        let height = self.tip.header.height + 1;
        let timestamp = now_secs().max(self.tip.header.timestamp + 1);
        let difficulty = self.expected_difficulty(&self.tip)?;

        // Include a buffered reveal for a commitment that expires at this height.
        let reveal = self
            .state
            .pending
            .iter()
            .filter(|(_, p)| p.expires_at == height)
            .find_map(|(id, _)| self.reveal_buffer.get(id).cloned());

        let mut block = Block {
            header: BlockHeader {
                version: 1,
                height,
                prev_hash: self.tip_hash,
                timestamp,
                difficulty,
                tx_root: tx_root(&transactions),
                state_root: [0u8; 32],
                reveal_root: reveal_root(&reveal),
            },
            solution: Some(sol),
            transactions,
            reveal,
        };

        // Execute against a scratch copy to obtain the post-state root.
        let mut state = self.state.clone();
        if let Err(err) = apply_block(&mut state, &block, &self.params) {
            // A buffered reveal that turns out to be invalid must not stop block
            // production: drop it and retry once without any reveal.
            if block.reveal.is_some() {
                tracing::warn!("dropping unusable reveal from candidate block: {err:#}");
                block.reveal = None;
                block.header.reveal_root = reveal_root(&None);
                state = self.state.clone();
                apply_block(&mut state, &block, &self.params)?;
            } else {
                return Err(err);
            }
        }
        block.header.state_root = state.root();
        Ok(block)
    }

    /// Entry point used by the RPC layer when a miner submits a solution.
    pub fn submit_solution(&mut self, sol: Solution) -> Result<Accepted> {
        let difficulty = self.expected_difficulty(&self.tip)?;
        if sol.nonce >= pouw::MAX_NONCE {
            bail!("nonce out of range");
        }
        let seed = pouw::task_seed(&self.tip_hash, &sol.miner, sol.salt);
        let pow = pouw::pow_hash(&seed, &sol.matmul_root, sol.nonce);
        if !meets_difficulty(&pow, difficulty) {
            bail!("solution does not satisfy the current difficulty {difficulty}");
        }
        let transactions = self.mempool.select(&self.state, &self.params);
        let block = self.build_block(sol, transactions)?;
        self.accept_block(block)
    }

    /// Store a reveal sent by a miner until a block can carry it.
    pub fn submit_reveal(&mut self, reveal: Reveal) -> Result<()> {
        let pending = match self.state.pending.get(&reveal.commit_id) {
            Some(p) => p.clone(),
            None => bail!(
                "no pending commitment {} (it may have already been settled or expired)",
                hex::encode(reveal.commit_id)
            ),
        };
        let expected_rows = pouw::challenge_rows(&self.tip_hash);
        if reveal.rows.len() != expected_rows.len() {
            bail!(
                "expected {} rows, got {}",
                expected_rows.len(),
                reveal.rows.len()
            );
        }
        // Cheap structural pre-check so obviously wrong reveals never reach a block.
        for (slot, row) in reveal.rows.iter().enumerate() {
            if row.index != expected_rows[slot] {
                bail!(
                    "row {slot} opens index {} but the challenge demands {}",
                    row.index,
                    expected_rows[slot]
                );
            }
            if row.values.len() != pouw::N {
                bail!("row {slot} must contain {} values", pouw::N);
            }
            let leaf = pouw::leaf_hash(row.index, &row.values);
            if !pouw::merkle_verify(&pending.matmul_root, row.index, &leaf, &row.proof) {
                bail!("row {slot} fails its Merkle inclusion proof");
            }
        }
        self.reveal_buffer.insert(reveal.commit_id, reveal);
        Ok(())
    }

    /// Commitments belonging to `miner` that still need to be opened, together
    /// with the rows the miner must publish.
    pub fn reveal_requests(&self, miner: &crate::types::Address) -> Vec<(Hash, Hash, Vec<u32>)> {
        let rows = pouw::challenge_rows(&self.tip_hash);
        self.state
            .pending
            .iter()
            .filter(|(id, p)| p.miner == *miner && !self.reveal_buffer.contains_key(*id))
            .map(|(id, p)| (*id, p.task_seed, rows.clone()))
            .collect()
    }
}
