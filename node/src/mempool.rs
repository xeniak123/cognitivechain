//! Transaction pool: signature-checked, nonce-ordered, fee-prioritised.

use crate::crypto::verify_transaction;
use crate::state::{ChainParams, State};
use crate::types::{Hash, Transaction, MAX_MEMO_LEN};
use anyhow::{bail, Result};
use std::collections::HashMap;

pub const MAX_POOL_SIZE: usize = 20_000;

#[derive(Default)]
pub struct Mempool {
    txs: HashMap<Hash, Transaction>,
}

impl Mempool {
    pub fn new() -> Self {
        Mempool::default()
    }

    pub fn len(&self) -> usize {
        self.txs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }

    pub fn contains(&self, hash: &Hash) -> bool {
        self.txs.contains_key(hash)
    }

    /// Admission control. Rejects anything that could not be included in a block
    /// built on the current state.
    pub fn insert(&mut self, tx: Transaction, state: &State, params: &ChainParams) -> Result<Hash> {
        if self.txs.len() >= MAX_POOL_SIZE {
            bail!("mempool is full ({MAX_POOL_SIZE} transactions)");
        }
        if tx.memo.len() > MAX_MEMO_LEN {
            bail!("memo too long: {} > {MAX_MEMO_LEN}", tx.memo.len());
        }
        if tx.fee < params.min_tx_fee {
            bail!("fee {} below minimum {}", tx.fee, params.min_tx_fee);
        }
        if tx.amount == 0 {
            bail!("amount must be greater than zero");
        }
        verify_transaction(&tx)?;

        let from = tx.from();
        if from == tx.to {
            bail!("self-transfer is not allowed");
        }
        let expected = state.nonce(&from);
        if tx.nonce < expected {
            bail!(
                "nonce {} is already used (account is at {expected})",
                tx.nonce
            );
        }
        // Allow a small forward gap so wallets can pipeline transactions.
        if tx.nonce > expected + 16 {
            bail!("nonce {} is too far ahead of {expected}", tx.nonce);
        }
        let total = tx
            .amount
            .checked_add(tx.fee)
            .ok_or_else(|| anyhow::anyhow!("amount + fee overflows u64"))?;
        if state.balance(&from) < total {
            bail!(
                "insufficient balance: {} has {}, needs {total}",
                from,
                state.balance(&from)
            );
        }

        let hash = tx.hash();
        self.txs.insert(hash, tx);
        Ok(hash)
    }

    /// Select a block's worth of transactions: highest fee first, but always in
    /// per-sender nonce order so the batch applies cleanly.
    pub fn select(&self, state: &State, params: &ChainParams) -> Vec<Transaction> {
        let mut candidates: Vec<&Transaction> = self.txs.values().collect();
        candidates.sort_by(|a, b| {
            b.fee
                .cmp(&a.fee)
                .then_with(|| a.nonce.cmp(&b.nonce))
                .then_with(|| a.hash().cmp(&b.hash()))
        });

        let mut next_nonce: HashMap<_, u64> = HashMap::new();
        let mut spent: HashMap<_, u64> = HashMap::new();
        let mut chosen = Vec::new();

        // Several passes let dependent transactions (nonce n, n+1, ...) be picked
        // up even when the higher-fee successor was sorted first.
        for _ in 0..8 {
            let mut progressed = false;
            for tx in &candidates {
                if chosen.len() >= params.max_block_txs {
                    break;
                }
                let hash = tx.hash();
                if chosen.iter().any(|c: &Transaction| c.hash() == hash) {
                    continue;
                }
                let from = tx.from();
                let expected = *next_nonce.get(&from).unwrap_or(&state.nonce(&from));
                if tx.nonce != expected {
                    continue;
                }
                let already = *spent.get(&from).unwrap_or(&0);
                let total = match tx.amount.checked_add(tx.fee) {
                    Some(v) => v,
                    None => continue,
                };
                let need = match already.checked_add(total) {
                    Some(v) => v,
                    None => continue,
                };
                if state.balance(&from) < need {
                    continue;
                }
                next_nonce.insert(from, expected + 1);
                spent.insert(from, need);
                chosen.push((*tx).clone());
                progressed = true;
            }
            if !progressed || chosen.len() >= params.max_block_txs {
                break;
            }
        }
        chosen
    }

    /// Drop transactions that a newly accepted block made invalid.
    pub fn prune(&mut self, included: &[Transaction], state: &State) {
        for tx in included {
            self.txs.remove(&tx.hash());
        }
        self.txs.retain(|_, tx| tx.nonce >= state.nonce(&tx.from()));
    }

    pub fn all(&self) -> Vec<Transaction> {
        self.txs.values().cloned().collect()
    }
}
