//! End-to-end consensus tests: honest mining works, dishonest mining does not.
//!
//! These run a real chain against a temporary database, mine real blocks with
//! the real Proof-of-Useful-Work pipeline, and then attempt every cheat the
//! scheme is supposed to prevent.

use cog_node::chain::{Accepted, Chain};
use cog_node::genesis::{Allocation, GenesisConfig, Params};
use cog_node::pouw;
use cog_node::types::{meets_difficulty, Address, Reveal, RowProof, Solution};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let mut path = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!("cogchain-test-{tag}-{unique}"));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_genesis(miner: Address) -> GenesisConfig {
    GenesisConfig {
        chain_id: "cogchain-test".into(),
        genesis_time: 1_700_000_000,
        params: Params {
            max_supply_acog: "100000000000000000".into(),
            initial_block_reward_acog: "4500000000".into(),
            halving_interval_tasks: 10_000_000,
            target_block_time_secs: 30,
            retarget_interval: 60,
            // Difficulty 1 means every nonce is a winner, so the tests spend
            // their time on the tensor work rather than on the hash search.
            initial_difficulty: 1,
            min_tx_fee_acog: "10000".into(),
            max_block_txs: 4096,
            max_future_drift_secs: 120,
        },
        allocations: vec![Allocation {
            label: "treasury".into(),
            address: miner,
            amount_acog: "10000000000000000".into(),
        }],
    }
}

/// One honest task: returns the solution plus the full product matrix so the
/// caller can build (or corrupt) the reveal.
fn solve(
    prev_hash: [u8; 32],
    miner: Address,
    difficulty: u64,
) -> (Solution, Vec<Vec<u16>>, Vec<[u8; 32]>) {
    for salt in 0u64..1024 {
        let seed = pouw::task_seed(&prev_hash, &miner, salt);
        let a = pouw::gen_matrix_a(&seed);
        let b = pouw::gen_matrix_b(&seed);
        let rows = pouw::matmul_full(&a, &b);
        let leaves: Vec<[u8; 32]> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| pouw::leaf_hash(i as u32, r))
            .collect();
        let root = pouw::merkle_root(&leaves);
        for nonce in 0..pouw::MAX_NONCE {
            if meets_difficulty(&pouw::pow_hash(&seed, &root, nonce), difficulty) {
                return (
                    Solution {
                        miner,
                        salt,
                        nonce,
                        matmul_root: root,
                    },
                    rows,
                    leaves,
                );
            }
        }
    }
    panic!("no solution found in the search budget");
}

fn build_reveal(
    commit_id: [u8; 32],
    challenge_seed: [u8; 32],
    rows: &[Vec<u16>],
    leaves: &[[u8; 32]],
) -> Reveal {
    let indices = pouw::challenge_rows(&challenge_seed);
    Reveal {
        commit_id,
        rows: indices
            .iter()
            .map(|&idx| RowProof {
                index: idx,
                values: rows[idx as usize].clone(),
                proof: pouw::merkle_proof(leaves, idx as usize),
            })
            .collect(),
    }
}

#[test]
fn honest_mining_mints_exactly_one_reward_per_verified_task() {
    let miner = Address([0x42; 20]);
    let dir = TempDir::new("honest");
    let cfg = test_genesis(miner);
    let mut chain = Chain::open(&dir.0, cfg).unwrap();

    let start_balance = chain.state.balance(&miner);
    let start_minted = chain.state.minted;

    // Block 1: commit only. Nothing is minted yet.
    let prev = chain.tip_hash;
    let (sol1, rows1, leaves1) = solve(prev, miner, chain.tip.header.difficulty);
    let seed1 = pouw::task_seed(&prev, &miner, sol1.salt);
    let commit1 = pouw::commit_id(&seed1, &sol1.matmul_root, sol1.nonce);
    matches!(
        chain.submit_solution(sol1).unwrap(),
        Accepted::Extended { .. }
    );

    assert_eq!(chain.tip.header.height, 1);
    assert_eq!(
        chain.state.minted, start_minted,
        "a commitment alone must not mint anything"
    );
    assert_eq!(chain.state.pending.len(), 1);

    // The challenge only exists now that block 1 has a hash.
    let block1_hash = chain.tip_hash;
    chain
        .submit_reveal(build_reveal(commit1, block1_hash, &rows1, &leaves1))
        .expect("an honest reveal must be accepted");

    // Block 2 carries the reveal, which settles the reward for block 1.
    let (sol2, _, _) = solve(block1_hash, miner, chain.tip.header.difficulty);
    chain.submit_solution(sol2).unwrap();

    assert_eq!(chain.tip.header.height, 2);
    assert!(chain.tip.reveal.is_some(), "block 2 must carry the reveal");
    assert_eq!(chain.state.tasks_completed, 1);
    assert_eq!(
        chain.state.balance(&miner) - start_balance,
        4_500_000_000,
        "exactly one 45 COG reward must have been minted"
    );
    assert_eq!(chain.state.minted - start_minted, 4_500_000_000);
}

#[test]
fn a_reveal_with_a_forged_row_is_rejected() {
    let miner = Address([0x43; 20]);
    let dir = TempDir::new("forged-row");
    let mut chain = Chain::open(&dir.0, test_genesis(miner)).unwrap();

    let prev = chain.tip_hash;
    let (sol, rows, leaves) = solve(prev, miner, chain.tip.header.difficulty);
    let seed = pouw::task_seed(&prev, &miner, sol.salt);
    let commit = pouw::commit_id(&seed, &sol.matmul_root, sol.nonce);
    chain.submit_solution(sol).unwrap();

    let block1_hash = chain.tip_hash;
    let mut reveal = build_reveal(commit, block1_hash, &rows, &leaves);
    // Flip a single field element out of the 32 * 1024 revealed.
    reveal.rows[7].values[500] ^= 1;

    let err = chain.submit_reveal(reveal).unwrap_err();
    assert!(
        err.to_string().contains("Merkle"),
        "a forged value must break the inclusion proof, got: {err}"
    );
    assert_eq!(chain.state.tasks_completed, 0);
}

#[test]
fn a_miner_that_skipped_rows_cannot_open_the_challenge() {
    // The economic core of the scheme: a miner that computed only some rows
    // has no valid opening for the rows it skipped, and forfeits the reward.
    let miner = Address([0x44; 20]);
    let dir = TempDir::new("skipped-rows");
    let mut chain = Chain::open(&dir.0, test_genesis(miner)).unwrap();

    let prev = chain.tip_hash;
    let (sol, rows, _) = solve(prev, miner, chain.tip.header.difficulty);
    let seed = pouw::task_seed(&prev, &miner, sol.salt);
    let commit = pouw::commit_id(&seed, &sol.matmul_root, sol.nonce);

    // A lazy miner commits to a tree where the second half of the rows is junk.
    let lazy_leaves: Vec<[u8; 32]> = (0..pouw::N)
        .map(|i| {
            if i < pouw::N / 2 {
                pouw::leaf_hash(i as u32, &rows[i])
            } else {
                pouw::leaf_hash(i as u32, &vec![0u16; pouw::N])
            }
        })
        .collect();
    let lazy_root = pouw::merkle_root(&lazy_leaves);
    assert_ne!(lazy_root, sol.matmul_root);

    chain.submit_solution(sol).unwrap();
    let block1_hash = chain.tip_hash;
    let indices = pouw::challenge_rows(&block1_hash);
    assert!(
        indices.iter().any(|&i| (i as usize) >= pouw::N / 2),
        "with 32 draws the challenge hits the skipped half with overwhelming probability"
    );

    // Opening against the committed (honest) root with junk rows fails, and so
    // does opening against the lazy root, which was never committed.
    let cheat = Reveal {
        commit_id: commit,
        rows: indices
            .iter()
            .map(|&idx| RowProof {
                index: idx,
                values: if (idx as usize) < pouw::N / 2 {
                    rows[idx as usize].clone()
                } else {
                    vec![0u16; pouw::N]
                },
                proof: pouw::merkle_proof(&lazy_leaves, idx as usize),
            })
            .collect(),
    };
    assert!(chain.submit_reveal(cheat).is_err());
    assert_eq!(chain.state.tasks_completed, 0);
    assert_eq!(chain.state.balance(&miner), 10_000_000_000_000_000);
}

#[test]
fn an_unopened_commitment_forfeits_its_reward() {
    let miner = Address([0x45; 20]);
    let dir = TempDir::new("unopened");
    let mut chain = Chain::open(&dir.0, test_genesis(miner)).unwrap();
    let before = chain.state.balance(&miner);

    let prev = chain.tip_hash;
    let (sol1, _, _) = solve(prev, miner, chain.tip.header.difficulty);
    chain.submit_solution(sol1).unwrap();

    // Mine the next block without ever supplying the reveal for block 1.
    let (sol2, _, _) = solve(chain.tip_hash, miner, chain.tip.header.difficulty);
    chain.submit_solution(sol2).unwrap();

    assert_eq!(chain.tip.header.height, 2);
    assert!(chain.tip.reveal.is_none());
    assert_eq!(chain.state.tasks_completed, 0);
    assert_eq!(chain.state.balance(&miner), before, "nothing may be minted");
    assert_eq!(
        chain.state.pending.len(),
        1,
        "only the fresh commitment stays pending; the stale one was dropped"
    );
}

#[test]
fn a_solution_below_difficulty_is_rejected() {
    let miner = Address([0x46; 20]);
    let dir = TempDir::new("weak-pow");
    let mut cfg = test_genesis(miner);
    cfg.params.initial_difficulty = u64::MAX;
    let mut chain = Chain::open(&dir.0, cfg).unwrap();

    let bad = Solution {
        miner,
        salt: 1,
        nonce: 0,
        matmul_root: [0xAB; 32],
    };
    let err = chain.submit_solution(bad).unwrap_err();
    assert!(err.to_string().contains("difficulty"), "got: {err}");
    assert_eq!(chain.tip.header.height, 0);
}

#[test]
fn a_nonce_outside_the_bounded_space_is_rejected() {
    // Enforcing the nonce bound is what stops a miner from substituting hash
    // grinding for the tensor work.
    let miner = Address([0x47; 20]);
    let dir = TempDir::new("nonce-bound");
    let mut chain = Chain::open(&dir.0, test_genesis(miner)).unwrap();

    let bad = Solution {
        miner,
        salt: 1,
        nonce: pouw::MAX_NONCE,
        matmul_root: [0xCD; 32],
    };
    assert!(chain.submit_solution(bad).is_err());
}

/// Regression test for a chain-halting bug.
///
/// The retarget window is the `interval` blocks below the retarget height, so
/// its first block sits `interval - 1` steps back from the parent. Walking a
/// full `interval` steps runs off the end of the chain at the *first* retarget
/// and every node then refuses to produce or accept another block - the network
/// stops permanently at that height. This mines straight through two retarget
/// boundaries to prove it does not.
#[test]
fn the_chain_survives_its_retarget_boundaries() {
    let miner = Address([0x49; 20]);
    let dir = TempDir::new("retarget");
    let mut cfg = test_genesis(miner);
    // A short window keeps the test fast; the boundary logic is identical.
    cfg.params.retarget_interval = 4;
    cfg.params.target_block_time_secs = 10;
    let mut chain = Chain::open(&dir.0, cfg).unwrap();

    let mut pending: Option<([u8; 32], Vec<Vec<u16>>, Vec<[u8; 32]>)> = None;

    for expected_height in 1..=9u64 {
        let difficulty = chain
            .expected_difficulty(&chain.tip)
            .unwrap_or_else(|e| panic!("difficulty unavailable at height {expected_height}: {e}"));

        let prev = chain.tip_hash;
        let (sol, rows, leaves) = solve(prev, miner, difficulty);
        let seed = pouw::task_seed(&prev, &miner, sol.salt);
        let commit = pouw::commit_id(&seed, &sol.matmul_root, sol.nonce);

        // Open the previous block's commitment so rewards actually settle.
        if let Some((id, prev_rows, prev_leaves)) = pending.take() {
            chain
                .submit_reveal(build_reveal(id, prev, &prev_rows, &prev_leaves))
                .expect("reveal must be accepted");
        }

        chain
            .submit_solution(sol)
            .unwrap_or_else(|e| panic!("block {expected_height} rejected: {e}"));
        assert_eq!(chain.tip.header.height, expected_height);
        pending = Some((commit, rows, leaves));
    }

    assert_eq!(chain.tip.header.height, 9);
    assert!(
        chain.state.tasks_completed >= 8,
        "rewards must keep settling across retargets, got {}",
        chain.state.tasks_completed
    );
    // Block timestamps only ever advance one second at a time here, which is far
    // faster than the 10 s target, so difficulty must have been pushed upward.
    assert!(
        chain.tip.header.difficulty > 1,
        "difficulty should have retargeted upward, still {}",
        chain.tip.header.difficulty
    );
}

#[test]
fn the_supply_cap_holds_across_the_whole_emission_schedule() {
    let cfg = test_genesis(Address([0x48; 20]));
    let initial = cfg.initial_reward().unwrap();
    let interval = cfg.params.halving_interval_tasks;
    let mut total: u128 = cfg.premine_total().unwrap() as u128;
    for epoch in 0..64u32 {
        let reward = initial >> epoch;
        if reward == 0 {
            break;
        }
        total += reward as u128 * interval as u128;
    }
    assert!(
        total <= cfg.max_supply().unwrap() as u128,
        "emission {total} must stay under the 1,000,000,000 COG cap"
    );
}
