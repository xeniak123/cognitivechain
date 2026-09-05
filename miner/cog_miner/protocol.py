"""Consensus-critical primitives, mirroring `node/src/pouw.rs` byte for byte.

Every constant, domain tag and hash pre-image in this module is part of the
CognitiveChain protocol. Changing anything here makes the miner produce proofs
that the network will reject.
"""

from __future__ import annotations

import numpy as np
from blake3 import blake3

# --- protocol constants (must equal node/src/pouw.rs) -----------------------

N = 1024
N_LOG2 = 10
P = 65521
CHALLENGE_ROWS = 32
MAX_NONCE = 1 << 16

TASK_DOMAIN = b"cog/task/v1"
MATRIX_A_DOMAIN = b"cog/matA/v1"
MATRIX_B_DOMAIN = b"cog/matB/v1"
POW_DOMAIN = b"cog/pow/v1"
CHALLENGE_DOMAIN = b"cog/chal/v1"
COMMIT_DOMAIN = b"cog/commit/v1"
LEAF_TAG = b"\x00"
NODE_TAG = b"\x01"

TWO_256 = 1 << 256


def address_bytes(address: str) -> bytes:
    """Decode a `cog`-prefixed address into its 20 raw bytes."""
    body = address[3:] if address.startswith("cog") else address
    raw = bytes.fromhex(body)
    if len(raw) != 20:
        raise ValueError(f"address must be 20 bytes, got {len(raw)}")
    return raw


def task_seed(prev_hash: bytes, miner: bytes, salt: int) -> bytes:
    h = blake3()
    h.update(TASK_DOMAIN)
    h.update(prev_hash)
    h.update(miner)
    h.update(salt.to_bytes(8, "little"))
    return h.digest()


def gen_matrix(seed: bytes, domain: bytes) -> np.ndarray:
    """Expand a task seed into one N x N operand over GF(p), row-major.

    Two BLAKE3 XOF bytes per entry, little-endian, reduced mod p.
    """
    h = blake3()
    h.update(domain)
    h.update(seed)
    stream = h.digest(length=2 * N * N)
    raw = np.frombuffer(stream, dtype="<u2").astype(np.uint32)
    return (raw % P).astype(np.uint16).reshape(N, N)


def gen_matrix_a(seed: bytes) -> np.ndarray:
    return gen_matrix(seed, MATRIX_A_DOMAIN)


def gen_matrix_b(seed: bytes) -> np.ndarray:
    return gen_matrix(seed, MATRIX_B_DOMAIN)


def leaf_hash(index: int, row: np.ndarray) -> bytes:
    h = blake3()
    h.update(LEAF_TAG)
    h.update(index.to_bytes(4, "little"))
    h.update(np.ascontiguousarray(row, dtype="<u2").tobytes())
    return h.digest()


def node_hash(left: bytes, right: bytes) -> bytes:
    h = blake3()
    h.update(NODE_TAG)
    h.update(left)
    h.update(right)
    return h.digest()


def build_leaves(c: np.ndarray) -> list[bytes]:
    return [leaf_hash(i, c[i]) for i in range(N)]


def merkle_levels(leaves: list[bytes]) -> list[list[bytes]]:
    """All tree levels, bottom (leaves) to top (root)."""
    levels = [leaves]
    level = leaves
    while len(level) > 1:
        level = [node_hash(level[i], level[i + 1]) for i in range(0, len(level), 2)]
        levels.append(level)
    return levels


def merkle_root(leaves: list[bytes]) -> bytes:
    return merkle_levels(leaves)[-1][0]


def merkle_proof(levels: list[list[bytes]], index: int) -> list[bytes]:
    """Bottom-up sibling path for `index`, N_LOG2 hashes long."""
    proof = []
    idx = index
    for level in levels[:-1]:
        sibling = idx + 1 if idx % 2 == 0 else idx - 1
        proof.append(level[sibling])
        idx //= 2
    return proof


def merkle_verify(root: bytes, index: int, leaf: bytes, proof: list[bytes]) -> bool:
    cur = leaf
    idx = index
    if len(proof) != N_LOG2 or not 0 <= index < N:
        return False
    for sibling in proof:
        cur = node_hash(cur, sibling) if idx % 2 == 0 else node_hash(sibling, cur)
        idx //= 2
    return cur == root


def pow_prefix(seed: bytes, root: bytes) -> blake3:
    """A pre-seeded hasher; `.copy()` it per nonce instead of rehashing."""
    h = blake3()
    h.update(POW_DOMAIN)
    h.update(seed)
    h.update(root)
    return h


def pow_hash(seed: bytes, root: bytes, nonce: int) -> bytes:
    h = pow_prefix(seed, root)
    h.update(nonce.to_bytes(8, "little"))
    return h.digest()


def commit_id(seed: bytes, root: bytes, nonce: int) -> bytes:
    h = blake3()
    h.update(COMMIT_DOMAIN)
    h.update(seed)
    h.update(root)
    h.update(nonce.to_bytes(8, "little"))
    return h.digest()


def meets_difficulty(digest: bytes, difficulty: int) -> bool:
    """`digest` interpreted big-endian, times difficulty, must stay below 2^256."""
    if difficulty <= 1:
        return True
    return int.from_bytes(digest, "big") * difficulty < TWO_256


def challenge_rows(commit_block_hash: bytes) -> list[int]:
    h = blake3()
    h.update(CHALLENGE_DOMAIN)
    h.update(commit_block_hash)
    stream = h.digest(length=4 * CHALLENGE_ROWS)
    raw = np.frombuffer(stream, dtype="<u4")
    return [int(v) % N for v in raw]


def search_nonce(seed: bytes, root: bytes, difficulty: int, limit: int = MAX_NONCE):
    """Scan the bounded nonce space. Returns (nonce, hash) or (None, None).

    The space is deliberately small: once it is exhausted the miner has to run a
    fresh matrix product, which is what keeps the work useful rather than pure
    hashing.
    """
    base = pow_prefix(seed, root)
    for nonce in range(limit):
        h = base.copy()
        h.update(nonce.to_bytes(8, "little"))
        digest = h.digest()
        if int.from_bytes(digest, "big") * difficulty < TWO_256:
            return nonce, digest
    return None, None
