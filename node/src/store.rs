//! Durable block storage on top of sled.

use crate::types::{Block, Hash};
use anyhow::{anyhow, Result};
use std::path::Path;

pub struct Store {
    db: sled::Db,
    blocks: sled::Tree,
    by_height: sled::Tree,
    work: sled::Tree,
    meta: sled::Tree,
    /// Post-state of selected blocks, so a restart does not have to re-execute
    /// the whole chain. Keyed by block hash, which makes a snapshot usable from
    /// any branch that contains that block.
    states: sled::Tree,
}

const TIP_KEY: &[u8] = b"tip";
const GENESIS_KEY: &[u8] = b"genesis";
const CHAIN_ID_KEY: &[u8] = b"chain_id";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = sled::open(path).map_err(|err| {
            // The raw error here is "could not acquire lock on ...", which tells
            // a user nothing about what to do. This is the single most likely
            // thing to go wrong when someone restarts a node, so say it plainly.
            let text = err.to_string();
            if text.contains("lock") {
                anyhow!(
                    "another cog-node is already using {}.

                     A data directory can only be opened by one node at a time.                      Close the other node and try again - if you just closed it,                      give it a few seconds to release the database. To run a                      second node, point it at a different --data-dir.",
                    path.display()
                )
            } else {
                anyhow!("cannot open database at {}: {text}", path.display())
            }
        })?;
        Ok(Store {
            blocks: db.open_tree("blocks")?,
            by_height: db.open_tree("by_height")?,
            work: db.open_tree("work")?,
            meta: db.open_tree("meta")?,
            states: db.open_tree("states")?,
            db,
        })
    }

    pub fn flush(&self) -> Result<()> {
        self.db.flush()?;
        Ok(())
    }

    pub fn put_block(&self, block: &Block, cumulative_work: u128) -> Result<Hash> {
        let hash = block.hash();
        let encoded = bincode::serialize(block)?;
        self.blocks.insert(hash, encoded)?;
        self.work.insert(hash, &cumulative_work.to_be_bytes())?;
        Ok(hash)
    }

    pub fn get_block(&self, hash: &Hash) -> Result<Option<Block>> {
        match self.blocks.get(hash)? {
            Some(raw) => Ok(Some(bincode::deserialize(&raw)?)),
            None => Ok(None),
        }
    }

    pub fn has_block(&self, hash: &Hash) -> Result<bool> {
        Ok(self.blocks.contains_key(hash)?)
    }

    pub fn cumulative_work(&self, hash: &Hash) -> Result<Option<u128>> {
        match self.work.get(hash)? {
            Some(raw) => {
                let mut buf = [0u8; 16];
                if raw.len() != 16 {
                    return Err(anyhow!("corrupt work entry"));
                }
                buf.copy_from_slice(&raw);
                Ok(Some(u128::from_be_bytes(buf)))
            }
            None => Ok(None),
        }
    }

    /// Save the post-state of the block `hash`, tagged with its height so old
    /// snapshots can be pruned.
    pub fn put_state(&self, hash: &Hash, height: u64, state: &[u8]) -> Result<()> {
        let mut value = Vec::with_capacity(8 + state.len());
        value.extend_from_slice(&height.to_be_bytes());
        value.extend_from_slice(state);
        self.states.insert(hash, value)?;
        Ok(())
    }

    /// The stored post-state of `hash`, if one was kept.
    pub fn get_state(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        match self.states.get(hash)? {
            Some(raw) if raw.len() > 8 => Ok(Some(raw[8..].to_vec())),
            Some(_) => Err(anyhow!("corrupt state snapshot")),
            None => Ok(None),
        }
    }

    /// Drop snapshots older than `keep_below`, so the store does not grow
    /// without bound. Recent snapshots are what a reorganisation needs.
    pub fn prune_states(&self, keep_below: u64) -> Result<()> {
        let mut stale = Vec::new();
        for item in self.states.iter() {
            let (key, value) = item?;
            if value.len() < 8 {
                stale.push(key);
                continue;
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&value[..8]);
            if u64::from_be_bytes(b) < keep_below {
                stale.push(key);
            }
        }
        for key in stale {
            self.states.remove(key)?;
        }
        Ok(())
    }

    /// Record `hash` as the canonical block at `height`.
    pub fn set_canonical(&self, height: u64, hash: &Hash) -> Result<()> {
        self.by_height.insert(height.to_be_bytes(), hash)?;
        Ok(())
    }

    pub fn canonical_at(&self, height: u64) -> Result<Option<Hash>> {
        match self.by_height.get(height.to_be_bytes())? {
            Some(raw) => {
                let mut h = [0u8; 32];
                if raw.len() != 32 {
                    return Err(anyhow!("corrupt height index"));
                }
                h.copy_from_slice(&raw);
                Ok(Some(h))
            }
            None => Ok(None),
        }
    }

    /// Remove canonical index entries above `height` (used when reorganising).
    pub fn truncate_canonical_above(&self, height: u64) -> Result<()> {
        let mut to_remove = Vec::new();
        for item in self.by_height.iter() {
            let (key, _) = item?;
            let mut b = [0u8; 8];
            b.copy_from_slice(&key);
            let h = u64::from_be_bytes(b);
            if h > height {
                to_remove.push(key);
            }
        }
        for key in to_remove {
            self.by_height.remove(key)?;
        }
        Ok(())
    }

    pub fn set_tip(&self, hash: &Hash) -> Result<()> {
        self.meta.insert(TIP_KEY, hash)?;
        Ok(())
    }

    pub fn tip(&self) -> Result<Option<Hash>> {
        Ok(self.meta.get(TIP_KEY)?.map(|raw| {
            let mut h = [0u8; 32];
            h.copy_from_slice(&raw);
            h
        }))
    }

    pub fn set_genesis(&self, hash: &Hash) -> Result<()> {
        self.meta.insert(GENESIS_KEY, hash)?;
        Ok(())
    }

    pub fn genesis(&self) -> Result<Option<Hash>> {
        Ok(self.meta.get(GENESIS_KEY)?.map(|raw| {
            let mut h = [0u8; 32];
            h.copy_from_slice(&raw);
            h
        }))
    }

    pub fn set_chain_id(&self, chain_id: &str) -> Result<()> {
        self.meta.insert(CHAIN_ID_KEY, chain_id.as_bytes())?;
        Ok(())
    }

    pub fn chain_id(&self) -> Result<Option<String>> {
        match self.meta.get(CHAIN_ID_KEY)? {
            Some(raw) => Ok(Some(String::from_utf8(raw.to_vec())?)),
            None => Ok(None),
        }
    }
}
