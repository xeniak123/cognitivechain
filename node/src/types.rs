//! Core protocol types for CognitiveChain.
//!
//! Everything in this module is consensus-critical: the binary encoding produced
//! here (via `bincode`) and the hash pre-images defined here are part of the
//! protocol and must match the miner implementation byte for byte.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// 32-byte BLAKE3 digest used for every hash in the protocol.
pub type Hash = [u8; 32];

pub const ZERO_HASH: Hash = [0u8; 32];

/// Smallest indivisible unit of COG. 1 COG = 100_000_000 acog.
pub const DECIMALS: u32 = 8;
pub const COG: u64 = 100_000_000;

/// Human readable prefix for addresses: `cog` + 40 hex chars.
pub const ADDR_PREFIX: &str = "cog";

/// A 20-byte account address: first 20 bytes of BLAKE3(ed25519 public key).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Address(pub [u8; 20]);

impl Address {
    pub fn from_pubkey(pk: &[u8; 32]) -> Self {
        let digest = blake3::hash(pk);
        let mut out = [0u8; 20];
        out.copy_from_slice(&digest.as_bytes()[..20]);
        Address(out)
    }

    pub fn to_hex(&self) -> String {
        format!("{}{}", ADDR_PREFIX, hex::encode(self.0))
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let body = s.strip_prefix(ADDR_PREFIX).unwrap_or(s);
        let raw = hex::decode(body).map_err(|e| format!("address is not hex: {e}"))?;
        if raw.len() != 20 {
            return Err(format!("address must be 20 bytes, got {}", raw.len()));
        }
        let mut out = [0u8; 20];
        out.copy_from_slice(&raw);
        Ok(Address(out))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl Serialize for Address {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

struct AddressVisitor;

impl<'de> Visitor<'de> for AddressVisitor {
    type Value = Address;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a cog-prefixed 20-byte hex address")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Address, E> {
        Address::parse(v).map_err(E::custom)
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Address, D::Error> {
        d.deserialize_str(AddressVisitor)
    }
}

/// serde support for the 64-byte ed25519 signature.
///
/// `serde` only ships array impls up to length 32, so the signature is encoded
/// as a hex string. This is part of the wire format on both the P2P (bincode)
/// and RPC (JSON) paths.
mod sig_serde {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        let text = String::deserialize(d)?;
        let raw = hex::decode(&text).map_err(D::Error::custom)?;
        if raw.len() != 64 {
            return Err(D::Error::custom(format!(
                "signature must be 64 bytes, got {}",
                raw.len()
            )));
        }
        let mut out = [0u8; 64];
        out.copy_from_slice(&raw);
        Ok(out)
    }
}

/// A value transfer / fee-bearing transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Transaction {
    /// ed25519 public key of the sender; the sender address is derived from it.
    pub pubkey: [u8; 32],
    pub to: Address,
    pub amount: u64,
    pub fee: u64,
    /// Must equal the sender account's current nonce.
    pub nonce: u64,
    /// Free-form payload, capped by `MAX_MEMO_LEN`.
    pub memo: Vec<u8>,
    #[serde(with = "sig_serde")]
    pub signature: [u8; 64],
}

pub const MAX_MEMO_LEN: usize = 256;

impl Transaction {
    pub fn from(&self) -> Address {
        Address::from_pubkey(&self.pubkey)
    }

    /// Canonical bytes that are actually signed. Never includes the signature.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(96 + self.memo.len());
        buf.extend_from_slice(b"cog/tx/v1");
        buf.extend_from_slice(&self.pubkey);
        buf.extend_from_slice(&self.to.0);
        buf.extend_from_slice(&self.amount.to_le_bytes());
        buf.extend_from_slice(&self.fee.to_le_bytes());
        buf.extend_from_slice(&self.nonce.to_le_bytes());
        buf.extend_from_slice(&(self.memo.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.memo);
        buf
    }

    pub fn hash(&self) -> Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.signing_bytes());
        hasher.update(&self.signature);
        *hasher.finalize().as_bytes()
    }
}

/// The Proof-of-Useful-Work commitment produced by a miner.
///
/// The miner picks a private `salt`, which together with the parent hash and its
/// own address deterministically defines a unique matrix-multiplication task.
/// `matmul_root` is the Merkle root over the rows of C = A*B mod p.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Solution {
    pub miner: Address,
    pub salt: u64,
    pub nonce: u64,
    pub matmul_root: Hash,
}

/// One revealed row of the product matrix together with its Merkle inclusion proof.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RowProof {
    pub index: u32,
    /// N entries of C, each reduced mod p (fits in u16 because p < 2^16).
    pub values: Vec<u16>,
    /// Bottom-up sibling hashes, log2(N) of them.
    pub proof: Vec<Hash>,
}

/// The delayed opening of a previously committed solution.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reveal {
    /// Identifier of the commitment being opened; see `pouw::commit_id`.
    pub commit_id: Hash,
    pub rows: Vec<RowProof>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockHeader {
    pub version: u16,
    pub height: u64,
    pub prev_hash: Hash,
    /// Unix seconds.
    pub timestamp: u64,
    /// Work parameter; a hash h is valid iff `h_as_u256 * difficulty < 2^256`.
    pub difficulty: u64,
    pub tx_root: Hash,
    pub state_root: Hash,
    /// Commitment to the reveal payload (ZERO_HASH when the block carries none).
    pub reveal_root: Hash,
}

impl BlockHeader {
    /// Pre-image of the block hash. The solution is hashed in separately by
    /// `Block::hash` so that the PoW pre-image can be built without it.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(160);
        buf.extend_from_slice(b"cog/header/v1");
        buf.extend_from_slice(&self.version.to_le_bytes());
        buf.extend_from_slice(&self.height.to_le_bytes());
        buf.extend_from_slice(&self.prev_hash);
        buf.extend_from_slice(&self.timestamp.to_le_bytes());
        buf.extend_from_slice(&self.difficulty.to_le_bytes());
        buf.extend_from_slice(&self.tx_root);
        buf.extend_from_slice(&self.state_root);
        buf.extend_from_slice(&self.reveal_root);
        buf
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Block {
    pub header: BlockHeader,
    /// `None` only for the genesis block.
    pub solution: Option<Solution>,
    pub transactions: Vec<Transaction>,
    pub reveal: Option<Reveal>,
}

impl Block {
    pub fn hash(&self) -> Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.header.encode());
        match &self.solution {
            Some(sol) => {
                hasher.update(&[1u8]);
                hasher.update(&sol.miner.0);
                hasher.update(&sol.salt.to_le_bytes());
                hasher.update(&sol.nonce.to_le_bytes());
                hasher.update(&sol.matmul_root);
            }
            None => {
                hasher.update(&[0u8]);
            }
        };
        *hasher.finalize().as_bytes()
    }

    pub fn height(&self) -> u64 {
        self.header.height
    }
}

/// Merkle root over transaction hashes (duplicate-last padding).
pub fn tx_root(transactions: &[Transaction]) -> Hash {
    if transactions.is_empty() {
        return ZERO_HASH;
    }
    let mut level: Vec<Hash> = transactions.iter().map(|t| t.hash()).collect();
    while level.len() > 1 {
        if level.len() % 2 == 1 {
            let last = *level.last().unwrap();
            level.push(last);
        }
        let mut next = Vec::with_capacity(level.len() / 2);
        for pair in level.chunks(2) {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&[0x01]);
            hasher.update(&pair[0]);
            hasher.update(&pair[1]);
            next.push(*hasher.finalize().as_bytes());
        }
        level = next;
    }
    level[0]
}

/// Commitment to the reveal payload, bound into the header.
pub fn reveal_root(reveal: &Option<Reveal>) -> Hash {
    match reveal {
        None => ZERO_HASH,
        Some(r) => {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"cog/reveal/v1");
            hasher.update(&r.commit_id);
            hasher.update(&(r.rows.len() as u32).to_le_bytes());
            for row in &r.rows {
                hasher.update(&row.index.to_le_bytes());
                for v in &row.values {
                    hasher.update(&v.to_le_bytes());
                }
                for p in &row.proof {
                    hasher.update(p);
                }
            }
            *hasher.finalize().as_bytes()
        }
    }
}

/// True iff `hash` satisfies the given difficulty, i.e. `hash * difficulty < 2^256`.
///
/// Exact 256x64 -> 320 bit multiplication with an overflow check; avoids a
/// big-integer dependency and is trivially reproducible in Python.
pub fn meets_difficulty(hash: &Hash, difficulty: u64) -> bool {
    if difficulty <= 1 {
        return true;
    }
    let mut limbs = [0u64; 4];
    for (i, limb) in limbs.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&hash[i * 8..i * 8 + 8]);
        *limb = u64::from_be_bytes(b);
    }
    let d = difficulty as u128;
    let mut carry: u128 = 0;
    for i in (0..4).rev() {
        let prod = limbs[i] as u128 * d + carry;
        carry = prod >> 64;
    }
    carry == 0
}

/// Format an amount in acog as a human-readable COG string.
pub fn format_cog(amount: u64) -> String {
    let whole = amount / COG;
    let frac = amount % COG;
    format!("{}.{:08}", whole, frac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_roundtrip() {
        let addr = Address([7u8; 20]);
        assert_eq!(Address::parse(&addr.to_hex()).unwrap(), addr);
    }

    #[test]
    fn difficulty_edges() {
        assert!(meets_difficulty(&[0u8; 32], u64::MAX));
        assert!(!meets_difficulty(&[0xffu8; 32], 2));
        let mut h = [0u8; 32];
        h[1] = 0x01;
        assert!(!meets_difficulty(&h, 1 << 16));
        assert!(meets_difficulty(&h, 1 << 15));
    }

    #[test]
    fn format_amounts() {
        assert_eq!(format_cog(150_000_000), "1.50000000");
    }
}
