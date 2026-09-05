//! Proof-of-Useful-Work: verifiable dense tensor computation.
//!
//! # The scheme
//!
//! Each miner derives a *private* task from `(parent_hash, miner_address, salt)`.
//! The task is a dense matrix product over the prime field GF(p):
//!
//! ```text
//!   A, B in GF(p)^(N x N)   derived deterministically from the task seed
//!   C = A * B mod p         the useful work: 2*N^3 arithmetic ops
//! ```
//!
//! The miner commits to `root = MerkleRoot(rows of C)` and then grinds a bounded
//! nonce space `[0, MAX_NONCE)` looking for
//!
//! ```text
//!   H(POW_DOMAIN || task_seed || root || nonce) * difficulty < 2^256
//! ```
//!
//! Because the nonce space is bounded, a miner that exhausts it *must* perform a
//! fresh matrix product (new salt -> new seed -> new A, B) to keep searching.
//! Hash grinding therefore cannot be substituted for the tensor work.
//!
//! # Why the work cannot be faked
//!
//! Verifying `C` in full would cost the same as producing it, so verification is
//! deferred by exactly one block (commit-reveal):
//!
//! 1. Block `T` carries the commitment `root`. At that moment the challenge is
//!    unknown, because it is derived from the *full* hash of block `T`, which
//!    also depends on transactions and on the reveal payload of other miners.
//! 2. Block `T+1` must carry a reveal: `CHALLENGE_ROWS` rows of `C` selected by
//!    `H(CHALLENGE_DOMAIN || block_T_hash)`, each with a Merkle inclusion proof
//!    against `root`.
//! 3. The validator recomputes only those rows: `O(k * N^2)` instead of
//!    `O(N^3)` -- a 32x saving at `N = 1024`, `k = 32`.
//!
//! A miner that only computed a fraction `f` of the rows honestly passes with
//! probability `f^k`. Saving even half the work costs a `2^-32` gamble, and a
//! failed reveal forfeits the entire block reward (it is never minted).

use crate::types::{Address, Hash};

/// Matrix dimension. 1024 keeps a task at ~2.1 GFLOP-equivalent, ~4 MiB of operands.
pub const N: usize = 1024;
/// log2(N); the depth of the row Merkle tree.
pub const N_LOG2: usize = 10;
/// Largest prime below 2^16. Keeps every field element in a u16 and every
/// dot-product accumulator below 2^42, i.e. exact in f64 on any GPU.
pub const P: u32 = 65521;
/// Number of rows challenged at reveal time.
pub const CHALLENGE_ROWS: usize = 32;
/// Bounded nonce space per matrix product.
pub const MAX_NONCE: u64 = 1 << 16;
/// A commitment must be opened in the immediately following block. Keeping the
/// window at exactly one block means the challenge seed is always the child
/// block's `prev_hash`, so no extra chain lookup is ever needed to verify it.
pub const REVEAL_WINDOW: u64 = 1;

const TASK_DOMAIN: &[u8] = b"cog/task/v1";
const MATRIX_A_DOMAIN: &[u8] = b"cog/matA/v1";
const MATRIX_B_DOMAIN: &[u8] = b"cog/matB/v1";
const POW_DOMAIN: &[u8] = b"cog/pow/v1";
const CHALLENGE_DOMAIN: &[u8] = b"cog/chal/v1";
const COMMIT_DOMAIN: &[u8] = b"cog/commit/v1";
const LEAF_TAG: u8 = 0x00;
const NODE_TAG: u8 = 0x01;

/// Deterministic per-miner task identifier.
pub fn task_seed(prev_hash: &Hash, miner: &Address, salt: u64) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(TASK_DOMAIN);
    hasher.update(prev_hash);
    hasher.update(&miner.0);
    hasher.update(&salt.to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// Expand a task seed into one of the two operand matrices, row-major.
///
/// The BLAKE3 extendable-output function gives 2 bytes per entry; each pair is
/// read little-endian and reduced mod p. Both the node (Rust) and the miner
/// (Python) run exactly this construction.
pub fn gen_matrix(seed: &Hash, domain: &[u8]) -> Vec<u16> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(seed);
    let mut xof = hasher.finalize_xof();
    let mut bytes = vec![0u8; 2 * N * N];
    xof.fill(&mut bytes);

    let mut out = vec![0u16; N * N];
    for (i, entry) in out.iter_mut().enumerate() {
        let raw = u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]) as u32;
        *entry = (raw % P) as u16;
    }
    out
}

pub fn gen_matrix_a(seed: &Hash) -> Vec<u16> {
    gen_matrix(seed, MATRIX_A_DOMAIN)
}

pub fn gen_matrix_b(seed: &Hash) -> Vec<u16> {
    gen_matrix(seed, MATRIX_B_DOMAIN)
}

/// Compute a single row of `C = A * B mod p`.
///
/// `a_row` is row `i` of A (length N); `b` is B in row-major order.
/// Accumulators stay below `N * (p-1)^2 < 2^42`, so u64 is exact.
pub fn matmul_row(a_row: &[u16], b: &[u16]) -> Vec<u16> {
    debug_assert_eq!(a_row.len(), N);
    debug_assert_eq!(b.len(), N * N);
    let mut acc = vec![0u64; N];
    for (k, &a_ik) in a_row.iter().enumerate() {
        if a_ik == 0 {
            continue;
        }
        let a = a_ik as u64;
        let b_row = &b[k * N..k * N + N];
        for (j, &b_kj) in b_row.iter().enumerate() {
            acc[j] += a * b_kj as u64;
        }
    }
    acc.iter().map(|&v| (v % P as u64) as u16).collect()
}

/// Full reference product. Only used by tests and by `cog-node selftest`;
/// consensus never needs it.
pub fn matmul_full(a: &[u16], b: &[u16]) -> Vec<Vec<u16>> {
    (0..N)
        .map(|i| matmul_row(&a[i * N..i * N + N], b))
        .collect()
}

/// Leaf hash for row `index` of C.
pub fn leaf_hash(index: u32, values: &[u16]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[LEAF_TAG]);
    hasher.update(&index.to_le_bytes());
    let mut buf = Vec::with_capacity(values.len() * 2);
    for v in values {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    hasher.update(&buf);
    *hasher.finalize().as_bytes()
}

fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[NODE_TAG]);
    hasher.update(left);
    hasher.update(right);
    *hasher.finalize().as_bytes()
}

/// Merkle root over exactly `N` leaves (N is a power of two, so no padding).
pub fn merkle_root(leaves: &[Hash]) -> Hash {
    assert_eq!(leaves.len(), N, "row tree must have exactly N leaves");
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len() / 2);
        for pair in level.chunks(2) {
            next.push(node_hash(&pair[0], &pair[1]));
        }
        level = next;
    }
    level[0]
}

/// Recompute a Merkle root from a leaf and its bottom-up sibling path.
pub fn merkle_verify(root: &Hash, index: u32, leaf: &Hash, proof: &[Hash]) -> bool {
    if proof.len() != N_LOG2 {
        return false;
    }
    if index as usize >= N {
        return false;
    }
    let mut cur = *leaf;
    let mut idx = index as usize;
    for sibling in proof {
        cur = if idx.is_multiple_of(2) {
            node_hash(&cur, sibling)
        } else {
            node_hash(sibling, &cur)
        };
        idx /= 2;
    }
    &cur == root
}

/// The PoW pre-image. Bound to the task seed so that the nonce search is
/// worthless without the committed matrix product.
pub fn pow_hash(seed: &Hash, root: &Hash, nonce: u64) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(POW_DOMAIN);
    hasher.update(seed);
    hasher.update(root);
    hasher.update(&nonce.to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// Stable identifier of a commitment.
///
/// Deliberately independent of the block header: the header commits to the
/// state root, which in turn commits to the pending-commitment table, so a
/// header-derived key would be circular.
pub fn commit_id(seed: &Hash, matmul_root: &Hash, nonce: u64) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(COMMIT_DOMAIN);
    hasher.update(seed);
    hasher.update(matmul_root);
    hasher.update(&nonce.to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// Row indices that must be opened, given the hash of the block that carried
/// the commitment.
///
/// That hash covers the block's timestamp, its transaction set and the reveal
/// payload of the *previous* miner, none of which the committing miner controls,
/// so the challenge is unpredictable at commit time.
pub fn challenge_rows(commit_block_hash: &Hash) -> Vec<u32> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CHALLENGE_DOMAIN);
    hasher.update(commit_block_hash);
    let mut xof = hasher.finalize_xof();
    let mut bytes = vec![0u8; 4 * CHALLENGE_ROWS];
    xof.fill(&mut bytes);

    let mut rows = Vec::with_capacity(CHALLENGE_ROWS);
    for i in 0..CHALLENGE_ROWS {
        let mut b = [0u8; 4];
        b.copy_from_slice(&bytes[4 * i..4 * i + 4]);
        rows.push(u32::from_le_bytes(b) % N as u32);
    }
    rows
}

/// Build the full sibling path for row `index`.
pub fn merkle_proof(leaves: &[Hash], index: usize) -> Vec<Hash> {
    assert_eq!(leaves.len(), N);
    let mut proof = Vec::with_capacity(N_LOG2);
    let mut level = leaves.to_vec();
    let mut idx = index;
    while level.len() > 1 {
        let sibling = if idx.is_multiple_of(2) {
            idx + 1
        } else {
            idx - 1
        };
        proof.push(level[sibling]);
        let mut next = Vec::with_capacity(level.len() / 2);
        for pair in level.chunks(2) {
            next.push(node_hash(&pair[0], &pair[1]));
        }
        level = next;
        idx /= 2;
    }
    proof
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::meets_difficulty;

    #[test]
    fn matrices_are_deterministic_and_in_field() {
        let seed = task_seed(&[3u8; 32], &Address([9u8; 20]), 42);
        let a = gen_matrix_a(&seed);
        let a2 = gen_matrix_a(&seed);
        assert_eq!(a, a2);
        assert_eq!(a.len(), N * N);
        assert!(a.iter().all(|&v| (v as u32) < P));
        let b = gen_matrix_b(&seed);
        assert_ne!(a, b, "A and B must be domain separated");
    }

    #[test]
    fn merkle_proofs_round_trip() {
        let seed = task_seed(&[1u8; 32], &Address([2u8; 20]), 7);
        let a = gen_matrix_a(&seed);
        let b = gen_matrix_b(&seed);
        // Only compute the rows we actually check, to keep the test fast.
        let mut leaves = vec![[0u8; 32]; N];
        let mut rows = std::collections::HashMap::new();
        for i in [0usize, 1, 5, N - 1] {
            let row = matmul_row(&a[i * N..i * N + N], &b);
            leaves[i] = leaf_hash(i as u32, &row);
            rows.insert(i, row);
        }
        // Remaining leaves are filled with the deterministic empty-row hash so the
        // tree is well formed; the proof relation is what is under test.
        for (i, leaf) in leaves.iter_mut().enumerate() {
            if !rows.contains_key(&i) {
                *leaf = leaf_hash(i as u32, &vec![0u16; N]);
            }
        }
        let root = merkle_root(&leaves);
        for (i, row) in &rows {
            let proof = merkle_proof(&leaves, *i);
            assert_eq!(proof.len(), N_LOG2);
            assert!(merkle_verify(
                &root,
                *i as u32,
                &leaf_hash(*i as u32, row),
                &proof
            ));
            assert!(!merkle_verify(
                &root,
                ((*i + 1) % N) as u32,
                &leaf_hash(*i as u32, row),
                &proof
            ));
        }
    }

    #[test]
    fn challenge_is_stable_and_in_range() {
        let rows = challenge_rows(&[0xabu8; 32]);
        assert_eq!(rows.len(), CHALLENGE_ROWS);
        assert!(rows.iter().all(|&r| (r as usize) < N));
        assert_eq!(rows, challenge_rows(&[0xabu8; 32]));
        assert_ne!(rows, challenge_rows(&[0xacu8; 32]));
    }

    #[test]
    fn trivial_difficulty_is_always_met() {
        let seed = task_seed(&[0u8; 32], &Address::default(), 0);
        let h = pow_hash(&seed, &[0u8; 32], 0);
        assert!(meets_difficulty(&h, 1));
    }

    #[test]
    fn small_matmul_matches_naive_reference() {
        // Verify matmul_row against an independent triple loop on a tiny slice.
        let seed = task_seed(&[5u8; 32], &Address([1u8; 20]), 1);
        let a = gen_matrix_a(&seed);
        let b = gen_matrix_b(&seed);
        let row = matmul_row(&a[0..N], &b);
        for j in [0usize, 1, 500, N - 1] {
            let mut acc: u64 = 0;
            for k in 0..N {
                acc += a[k] as u64 * b[k * N + j] as u64;
            }
            assert_eq!(row[j] as u64, acc % P as u64);
        }
    }
}
