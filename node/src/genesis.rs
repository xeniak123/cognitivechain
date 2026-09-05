//! Genesis configuration: chain parameters, initial allocation, emission policy.

use crate::types::{Address, Block, BlockHeader, Hash, ZERO_HASH};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Consensus parameters. Every field is fixed at genesis and enforced by all nodes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Params {
    /// Hard cap on total supply, in acog. Never exceeded, by construction.
    pub max_supply_acog: String,
    /// Block reward before the first halving, in acog.
    pub initial_block_reward_acog: String,
    /// Halving is driven by *verified useful-work tasks*, not by block height.
    pub halving_interval_tasks: u64,
    /// Desired seconds between blocks.
    pub target_block_time_secs: u64,
    /// Difficulty is recomputed every this many blocks.
    pub retarget_interval: u64,
    /// Difficulty of block 1.
    pub initial_difficulty: u64,
    /// Transactions below this fee are rejected from the mempool and from blocks.
    pub min_tx_fee_acog: String,
    /// Upper bound on transactions per block.
    pub max_block_txs: usize,
    /// Rejection threshold for block timestamps in the future, in seconds.
    pub max_future_drift_secs: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Allocation {
    pub label: String,
    pub address: Address,
    pub amount_acog: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenesisConfig {
    pub chain_id: String,
    pub genesis_time: u64,
    pub params: Params,
    pub allocations: Vec<Allocation>,
}

fn parse_amount(field: &str, raw: &str) -> Result<u64> {
    raw.parse::<u64>()
        .with_context(|| format!("`{field}` must be an integer amount in acog, got {raw:?}"))
}

impl GenesisConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read genesis file {}", path.display()))?;
        let cfg: GenesisConfig = serde_json::from_str(&text)
            .with_context(|| format!("cannot parse genesis file {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn max_supply(&self) -> Result<u64> {
        parse_amount("max_supply_acog", &self.params.max_supply_acog)
    }

    pub fn initial_reward(&self) -> Result<u64> {
        parse_amount(
            "initial_block_reward_acog",
            &self.params.initial_block_reward_acog,
        )
    }

    pub fn min_tx_fee(&self) -> Result<u64> {
        parse_amount("min_tx_fee_acog", &self.params.min_tx_fee_acog)
    }

    pub fn premine_total(&self) -> Result<u64> {
        let mut total: u64 = 0;
        for a in &self.allocations {
            let amt = parse_amount("amount_acog", &a.amount_acog)?;
            total = total
                .checked_add(amt)
                .ok_or_else(|| anyhow!("premine allocations overflow u64"))?;
        }
        Ok(total)
    }

    /// Structural sanity checks that must hold before the chain can start.
    pub fn validate(&self) -> Result<()> {
        if self.chain_id.is_empty() {
            bail!("chain_id must not be empty");
        }
        let max_supply = self.max_supply()?;
        let premine = self.premine_total()?;
        if premine > max_supply {
            bail!("premine ({premine} acog) exceeds max supply ({max_supply} acog)");
        }
        if self.params.halving_interval_tasks == 0 {
            bail!("halving_interval_tasks must be > 0");
        }
        if self.params.target_block_time_secs == 0 {
            bail!("target_block_time_secs must be > 0");
        }
        if self.params.retarget_interval == 0 {
            bail!("retarget_interval must be > 0");
        }
        if self.params.initial_difficulty == 0 {
            bail!("initial_difficulty must be > 0");
        }
        if self.params.max_block_txs == 0 {
            bail!("max_block_txs must be > 0");
        }

        // Total emission must fit under the cap: sum over halving epochs of
        // (reward >> e) * interval, which converges to 2 * initial * interval.
        let initial = self.initial_reward()? as u128;
        let interval = self.params.halving_interval_tasks as u128;
        let mut emission: u128 = 0;
        for epoch in 0..64u32 {
            let reward = initial >> epoch;
            if reward == 0 {
                break;
            }
            emission += reward * interval;
        }
        let projected = emission + premine as u128;
        if projected > max_supply as u128 {
            bail!(
                "emission schedule would mint {projected} acog including premine, \
                 which exceeds max_supply {max_supply} acog; lower \
                 initial_block_reward_acog or halving_interval_tasks"
            );
        }
        Ok(())
    }

    /// A duplicate address in the allocation list is almost always a config bug.
    pub fn assert_unique_allocations(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for a in &self.allocations {
            if !seen.insert(a.address) {
                bail!("duplicate allocation address {}", a.address);
            }
        }
        Ok(())
    }

    /// Deterministic identifier of this genesis document; two nodes with a
    /// different genesis hash can never agree on a chain.
    pub fn genesis_hash(&self) -> Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"cog/genesis/v1");
        hasher.update(self.chain_id.as_bytes());
        hasher.update(&self.genesis_time.to_le_bytes());
        hasher.update(self.params.max_supply_acog.as_bytes());
        hasher.update(self.params.initial_block_reward_acog.as_bytes());
        hasher.update(&self.params.halving_interval_tasks.to_le_bytes());
        hasher.update(&self.params.target_block_time_secs.to_le_bytes());
        hasher.update(&self.params.retarget_interval.to_le_bytes());
        hasher.update(&self.params.initial_difficulty.to_le_bytes());
        hasher.update(self.params.min_tx_fee_acog.as_bytes());
        hasher.update(&(self.params.max_block_txs as u64).to_le_bytes());
        let mut allocs = self.allocations.clone();
        allocs.sort_by_key(|a| a.address);
        for a in &allocs {
            hasher.update(&a.address.0);
            hasher.update(a.amount_acog.as_bytes());
        }
        *hasher.finalize().as_bytes()
    }

    /// Build the genesis block. `state_root` is supplied by the state machine
    /// after applying the allocations.
    pub fn genesis_block(&self, state_root: Hash) -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                height: 0,
                prev_hash: self.genesis_hash(),
                timestamp: self.genesis_time,
                difficulty: self.params.initial_difficulty,
                tx_root: ZERO_HASH,
                state_root,
                reveal_root: ZERO_HASH,
            },
            solution: None,
            transactions: Vec::new(),
            reveal: None,
        }
    }
}

/// Block reward for the next task, given how many tasks have already been
/// verified. Returns 0 once the schedule is exhausted.
pub fn block_reward(initial_reward: u64, halving_interval_tasks: u64, tasks_completed: u64) -> u64 {
    let epoch = tasks_completed / halving_interval_tasks;
    if epoch >= 64 {
        return 0;
    }
    initial_reward >> epoch
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GenesisConfig {
        GenesisConfig {
            chain_id: "cognitivechain-1".into(),
            genesis_time: 1_700_000_000,
            params: Params {
                max_supply_acog: "100000000000000000".into(), // 1e9 COG
                initial_block_reward_acog: "4500000000".into(), // 45 COG
                halving_interval_tasks: 10_000_000,
                target_block_time_secs: 30,
                retarget_interval: 60,
                initial_difficulty: 1024,
                min_tx_fee_acog: "10000".into(),
                max_block_txs: 4096,
                max_future_drift_secs: 120,
            },
            allocations: vec![Allocation {
                label: "founders".into(),
                address: crate::types::Address([1u8; 20]),
                amount_acog: "10000000000000000".into(), // 100M COG
            }],
        }
    }

    #[test]
    fn schedule_fits_under_cap() {
        cfg().validate().unwrap();
    }

    #[test]
    fn oversized_reward_is_rejected() {
        let mut c = cfg();
        c.params.initial_block_reward_acog = "90000000000".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn halving_progression() {
        let initial = 4_500_000_000u64;
        assert_eq!(block_reward(initial, 10_000_000, 0), initial);
        assert_eq!(block_reward(initial, 10_000_000, 9_999_999), initial);
        assert_eq!(block_reward(initial, 10_000_000, 10_000_000), initial / 2);
        assert_eq!(block_reward(initial, 10_000_000, 20_000_000), initial / 4);
        assert_eq!(block_reward(initial, 10_000_000, 640_000_000), 0);
    }

    #[test]
    fn premine_over_cap_rejected() {
        let mut c = cfg();
        c.allocations[0].amount_acog = "100000000000000001".into();
        assert!(c.validate().is_err());
    }
}
