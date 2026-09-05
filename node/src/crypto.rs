//! Key management and signature verification.

use crate::types::{Address, Transaction};
use anyhow::{anyhow, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// An ed25519 keypair, persisted as a JSON keyfile.
#[derive(Clone, Serialize, Deserialize)]
pub struct Keypair {
    /// 32-byte ed25519 secret seed, hex encoded.
    pub secret: String,
    /// 32-byte ed25519 public key, hex encoded.
    pub public: String,
    /// Derived address, stored for readability only; always re-derived on load.
    pub address: String,
}

impl Keypair {
    pub fn generate() -> Self {
        let seed: [u8; 32] = rand::random();
        Self::from_seed(&seed)
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let sk = SigningKey::from_bytes(seed);
        let pk = sk.verifying_key();
        let address = Address::from_pubkey(&pk.to_bytes());
        Keypair {
            secret: hex::encode(seed),
            public: hex::encode(pk.to_bytes()),
            address: address.to_hex(),
        }
    }

    pub fn signing_key(&self) -> Result<SigningKey> {
        let raw = hex::decode(&self.secret)?;
        if raw.len() != 32 {
            return Err(anyhow!("secret key must be 32 bytes"));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&raw);
        Ok(SigningKey::from_bytes(&seed))
    }

    pub fn public_bytes(&self) -> Result<[u8; 32]> {
        let raw = hex::decode(&self.public)?;
        if raw.len() != 32 {
            return Err(anyhow!("public key must be 32 bytes"));
        }
        let mut pk = [0u8; 32];
        pk.copy_from_slice(&raw);
        Ok(pk)
    }

    pub fn address(&self) -> Result<Address> {
        Address::parse(&self.address).map_err(|e| anyhow!(e))
    }

    /// Build and sign a transfer transaction.
    pub fn sign_transfer(
        &self,
        chain_id: &str,
        to: Address,
        amount: u64,
        fee: u64,
        nonce: u64,
        memo: Vec<u8>,
    ) -> Result<Transaction> {
        let sk = self.signing_key()?;
        let mut tx = Transaction {
            chain_id: chain_id.to_string(),
            pubkey: self.public_bytes()?,
            to,
            amount,
            fee,
            nonce,
            memo,
            signature: [0u8; 64],
        };
        let sig = sk.sign(&tx.signing_bytes());
        tx.signature = sig.to_bytes();
        Ok(tx)
    }
}

/// Verify a transaction's ed25519 signature over its canonical signing bytes.
pub fn verify_transaction(tx: &Transaction) -> Result<()> {
    let vk = VerifyingKey::from_bytes(&tx.pubkey).map_err(|e| anyhow!("bad public key: {e}"))?;
    let sig = Signature::from_bytes(&tx.signature);
    vk.verify(&tx.signing_bytes(), &sig)
        .map_err(|e| anyhow!("invalid signature: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let kp = Keypair::generate();
        let tx = kp
            .sign_transfer(
                "test-chain",
                Address([1u8; 20]),
                500,
                1,
                0,
                b"hello".to_vec(),
            )
            .unwrap();
        verify_transaction(&tx).unwrap();
        assert_eq!(tx.from(), kp.address().unwrap());
    }

    #[test]
    fn a_transaction_from_another_chain_is_invalid() {
        let kp = Keypair::generate();
        let mut tx = kp
            .sign_transfer(
                "cognitivechain-devnet",
                Address([2u8; 20]),
                100,
                1,
                0,
                vec![],
            )
            .unwrap();
        verify_transaction(&tx).unwrap();
        // Replaying it on a different network must fail, not merely be rejected
        // by policy: the chain id is inside the signed bytes.
        tx.chain_id = "cognitivechain-1".into();
        assert!(verify_transaction(&tx).is_err());
    }

    #[test]
    fn tampered_amount_fails() {
        let kp = Keypair::generate();
        let mut tx = kp
            .sign_transfer("test-chain", Address([1u8; 20]), 500, 1, 0, vec![])
            .unwrap();
        tx.amount = 501;
        assert!(verify_transaction(&tx).is_err());
    }
}
