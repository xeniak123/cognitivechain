//! Durable block storage on top of sled.

use crate::types::{Block, Hash};
use anyhow::{anyhow, Context, Result};
use std::path::Path;

pub struct Store {
    db: sled::Db,
    blocks: sled::Tree,
    by_height: sled::Tree,
    work: sled::Tree,
    meta: sled::Tree,
}

const TIP_KEY: &[u8] = b"tip";
const GENESIS_KEY: &[u8] = b"genesis";
const CHAIN_ID_KEY: &[u8] = b"chain_id";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = sled::open(path)
            .with_context(|| format!("cannot open database at {}", path.display()))?;
        Ok(Store {
            blocks: db.open_tree("blocks")?,
            by_height: db.open_tree("by_height")?,
            work: db.open_tree("work")?,
            meta: db.open_tree("meta")?,
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
