//! CognitiveChain node and wallet CLI.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use cog_node::chain::{self, Chain};
use cog_node::client::rpc_call;
use cog_node::crypto;
use cog_node::genesis;
use cog_node::p2p;
use cog_node::pouw;
use cog_node::rpc;
use cog_node::state;
use cog_node::types;
use cog_node::wallet_ui;
use crypto::Keypair;
use genesis::{Allocation, GenesisConfig, Params};
use parking_lot::Mutex;
use serde_json::json;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use types::{format_cog, parse_cog, Address};

#[derive(Parser)]
#[command(
    name = "cog-node",
    version,
    about = "CognitiveChain (COG) Layer-1 node, wallet and toolbox"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a validating node: JSON-RPC for miners and wallets, TCP gossip for peers.
    Run {
        #[arg(long, default_value = "genesis.json")]
        genesis: PathBuf,
        #[arg(long, default_value = "./data")]
        data_dir: PathBuf,
        #[arg(long, default_value = "0.0.0.0:26657")]
        rpc: SocketAddr,
        #[arg(long, default_value = "0.0.0.0:26656")]
        p2p: SocketAddr,
        /// Repeatable: --peer host:port
        #[arg(long)]
        peer: Vec<String>,
    },
    /// Create a new ed25519 wallet keyfile.
    Keygen {
        #[arg(long, default_value = "wallet.json")]
        out: PathBuf,
    },
    /// Print the address of a keyfile.
    Address {
        #[arg(long, default_value = "wallet.json")]
        key: PathBuf,
    },
    /// Query an account balance through a node's RPC.
    Balance {
        #[arg(long)]
        address: String,
        #[arg(long, default_value = "127.0.0.1:26657")]
        rpc: String,
    },
    /// Sign and broadcast a transfer.
    Send {
        #[arg(long, default_value = "wallet.json")]
        key: PathBuf,
        #[arg(long)]
        to: String,
        /// Amount in COG, e.g. 12.5
        #[arg(long)]
        amount: String,
        /// Fee in COG.
        #[arg(long, default_value = "0.001")]
        fee: String,
        #[arg(long, default_value = "127.0.0.1:26657")]
        rpc: String,
    },
    /// Write a ready-to-edit genesis file.
    GenesisTemplate {
        #[arg(long, default_value = "genesis.json")]
        out: PathBuf,
        #[arg(long, default_value = "cognitivechain-1")]
        chain_id: String,
        /// Address receiving the founders allocation (5% of max supply).
        #[arg(long)]
        founders: String,
        /// Address receiving the ecosystem allocation (3%).
        #[arg(long)]
        ecosystem: String,
        /// Address receiving the liquidity allocation (2%).
        #[arg(long)]
        liquidity: String,
        /// Difficulty of block 1. Roughly `expected hashes per block`; the
        /// network retargets from here every `retarget_interval` blocks.
        /// Use a small value (e.g. 200000) for a local devnet.
        #[arg(long, default_value_t = 5_000_000)]
        initial_difficulty: u64,
        /// Seconds between blocks the retargeting algorithm aims for.
        #[arg(long, default_value_t = 30)]
        block_time: u64,
    },
    /// Open the graphical wallet in a browser. Signing happens locally; the
    /// page never receives the private key.
    WalletUi {
        #[arg(long, default_value = "wallet.json")]
        key: PathBuf,
        #[arg(long, default_value = "127.0.0.1:26658")]
        bind: SocketAddr,
        #[arg(long, default_value = "127.0.0.1:26657")]
        rpc: String,
        /// Allow binding to a non-loopback address. Anything that can reach the
        /// port can spend the wallet, so put authentication in front of it.
        #[arg(long)]
        allow_remote: bool,
        /// Do not open a browser; just print the address.
        #[arg(long)]
        no_open: bool,
    },
    /// Validate a genesis file and print the derived constants.
    InspectGenesis {
        #[arg(long, default_value = "genesis.json")]
        genesis: PathBuf,
    },
    /// Run one full useful-work task locally and verify it end to end.
    Selftest,
}

fn load_key(path: &PathBuf) -> Result<Keypair> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read keyfile {}", path.display()))?;
    let kp: Keypair = serde_json::from_str(&text)
        .with_context(|| format!("cannot parse keyfile {}", path.display()))?;
    // Re-derive so a hand-edited file cannot lie about its address.
    let derived = Keypair::from_seed(&{
        let raw = hex::decode(&kp.secret)?;
        if raw.len() != 32 {
            bail!("secret key must be 32 bytes");
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&raw);
        seed
    });
    Ok(derived)
}

async fn cmd_run(
    genesis_path: PathBuf,
    data_dir: PathBuf,
    rpc_addr: SocketAddr,
    p2p_addr: SocketAddr,
    peers: Vec<String>,
) -> Result<()> {
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;
    let cfg = GenesisConfig::load(&genesis_path)?;
    let chain = Chain::open(&data_dir, cfg)?;

    tracing::info!(
        "chain {} ready at height {} (genesis {})",
        chain.params.chain_id,
        chain.tip.header.height,
        hex::encode(chain.genesis_hash()?)
    );

    let chain = Arc::new(Mutex::new(chain));
    let (announce, _rx) = tokio::sync::broadcast::channel(256);

    let p2p_ctx = Arc::new(p2p::P2p {
        chain: chain.clone(),
        announce: announce.clone(),
    });
    let rpc_ctx = rpc::RpcContext {
        chain: chain.clone(),
        announce: announce.clone(),
    };

    let mut tasks = Vec::new();
    tasks.push(tokio::spawn({
        let ctx = p2p_ctx.clone();
        async move {
            if let Err(err) = p2p::listen(ctx, p2p_addr).await {
                tracing::error!("P2P listener stopped: {err:#}");
            }
        }
    }));
    for peer in peers {
        let ctx = p2p_ctx.clone();
        tasks.push(tokio::spawn(async move {
            p2p::dial_forever(ctx, peer).await;
        }));
    }
    tasks.push(tokio::spawn(async move {
        if let Err(err) = rpc::serve(rpc_ctx, rpc_addr).await {
            tracing::error!("RPC server stopped: {err:#}");
        }
    }));

    tokio::signal::ctrl_c().await.ok();
    tracing::info!("shutting down");
    for task in tasks {
        task.abort();
    }
    Ok(())
}

fn cmd_genesis_template(
    out: PathBuf,
    chain_id: String,
    founders: String,
    ecosystem: String,
    liquidity: String,
    initial_difficulty: u64,
    block_time: u64,
) -> Result<()> {
    let parse = |label: &str, s: &str| -> Result<Address> {
        Address::parse(s).map_err(|e| anyhow!("{label} address: {e}"))
    };
    let cfg = GenesisConfig {
        chain_id,
        genesis_time: chain::now_secs(),
        params: Params {
            // 1_000_000_000 COG hard cap.
            max_supply_acog: "100000000000000000".into(),
            // 45 COG per verified task; halves every 10M tasks.
            // Total emission converges to 900M COG, plus 100M premine = 1B.
            initial_block_reward_acog: "4500000000".into(),
            halving_interval_tasks: 10_000_000,
            target_block_time_secs: block_time,
            retarget_interval: 60,
            initial_difficulty,
            min_tx_fee_acog: "10000".into(),
            max_block_txs: 4096,
            max_future_drift_secs: 120,
        },
        allocations: vec![
            Allocation {
                label: "founders".into(),
                address: parse("founders", &founders)?,
                amount_acog: "5000000000000000".into(), // 50M COG (5%)
            },
            Allocation {
                label: "ecosystem".into(),
                address: parse("ecosystem", &ecosystem)?,
                amount_acog: "3000000000000000".into(), // 30M COG (3%)
            },
            Allocation {
                label: "liquidity".into(),
                address: parse("liquidity", &liquidity)?,
                amount_acog: "2000000000000000".into(), // 20M COG (2%)
            },
        ],
    };
    cfg.validate()?;
    cfg.assert_unique_allocations()?;
    std::fs::write(&out, serde_json::to_string_pretty(&cfg)? + "\n")?;
    println!("wrote {}", out.display());
    println!("genesis hash: {}", hex::encode(cfg.genesis_hash()));
    Ok(())
}

fn cmd_inspect_genesis(path: PathBuf) -> Result<()> {
    let cfg = GenesisConfig::load(&path)?;
    cfg.assert_unique_allocations()?;
    let state = state::genesis_state(&cfg)?;
    let now = chain::now_secs();
    let when = if cfg.genesis_time > now {
        format!(
            "{} (unix) - {} days in the FUTURE; no block can be produced before then",
            cfg.genesis_time,
            (cfg.genesis_time - now) / 86_400
        )
    } else {
        format!(
            "{} (unix) - in the past, the chain may start now",
            cfg.genesis_time
        )
    };
    println!("chain_id          : {}", cfg.chain_id);
    println!("genesis time      : {when}");
    println!("genesis hash      : {}", hex::encode(cfg.genesis_hash()));
    println!("genesis state root: {}", hex::encode(state.root()));
    println!("max supply        : {} COG", format_cog(cfg.max_supply()?));
    println!(
        "premine           : {} COG",
        format_cog(cfg.premine_total()?)
    );
    println!(
        "initial reward    : {} COG per verified task",
        format_cog(cfg.initial_reward()?)
    );
    println!(
        "halving every     : {} tasks",
        cfg.params.halving_interval_tasks
    );
    println!(
        "target block time : {} s (retarget every {} blocks)",
        cfg.params.target_block_time_secs, cfg.params.retarget_interval
    );
    for alloc in &cfg.allocations {
        println!(
            "  alloc {:<10} {} -> {} COG",
            alloc.label,
            alloc.address,
            format_cog(alloc.amount_acog.parse()?)
        );
    }
    Ok(())
}

fn cmd_selftest() -> Result<()> {
    use std::time::Instant;
    println!("CognitiveChain useful-work selftest");
    println!("  matrix dim N = {}, field prime p = {}", pouw::N, pouw::P);

    let miner = Address([0x11; 20]);
    let prev = [0x22u8; 32];
    let salt = 7u64;
    let seed = pouw::task_seed(&prev, &miner, salt);
    println!("  task seed    : {}", hex::encode(seed));

    let t0 = Instant::now();
    let a = pouw::gen_matrix_a(&seed);
    let b = pouw::gen_matrix_b(&seed);
    println!("  operands generated in {:?}", t0.elapsed());

    let t1 = Instant::now();
    let rows = pouw::matmul_full(&a, &b);
    let elapsed = t1.elapsed();
    let ops = 2.0 * (pouw::N as f64).powi(3);
    println!(
        "  C = A*B mod p computed in {:?} ({:.2} GOP/s single-threaded CPU)",
        elapsed,
        ops / elapsed.as_secs_f64() / 1e9
    );

    let leaves: Vec<_> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| pouw::leaf_hash(i as u32, r))
        .collect();
    let root = pouw::merkle_root(&leaves);
    println!("  matmul root  : {}", hex::encode(root));

    let commit_block_hash = [0x33u8; 32];
    let challenge = pouw::challenge_rows(&commit_block_hash);
    let t2 = Instant::now();
    for (slot, &idx) in challenge.iter().enumerate() {
        let i = idx as usize;
        let proof = pouw::merkle_proof(&leaves, i);
        let leaf = pouw::leaf_hash(idx, &rows[i]);
        if !pouw::merkle_verify(&root, idx, &leaf, &proof) {
            bail!("merkle proof for challenge slot {slot} (row {i}) failed");
        }
        let recomputed = pouw::matmul_row(&a[i * pouw::N..i * pouw::N + pouw::N], &b);
        if recomputed != rows[i] {
            bail!("recomputed row {i} differs from the committed row");
        }
    }
    println!(
        "  {} challenged rows verified in {:?} (validator cost)",
        challenge.len(),
        t2.elapsed()
    );
    println!(
        "  verification is {:.1}x cheaper than production",
        elapsed.as_secs_f64() / t2.elapsed().as_secs_f64()
    );
    println!("selftest OK");
    Ok(())
}

async fn cmd_send(
    key: PathBuf,
    to: String,
    amount: String,
    fee: String,
    rpc: String,
) -> Result<()> {
    let kp = load_key(&key)?;
    let to = Address::parse(&to).map_err(|e| anyhow!("recipient: {e}"))?;
    let amount = parse_cog(&amount).map_err(|e| anyhow!(e))?;
    let fee = parse_cog(&fee).map_err(|e| anyhow!(e))?;

    // The chain id is signed over, so take it from the node being paid.
    let status = rpc_call(&rpc, "cog_status", json!({})).await?;
    let chain_id = status
        .get("chain_id")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow!("node did not report a chain_id"))?
        .to_string();

    let info = rpc_call(&rpc, "cog_getBalance", json!({"address": kp.address})).await?;
    let nonce = info
        .get("nonce")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("node did not return a nonce"))?;

    let tx = kp.sign_transfer(&chain_id, to, amount, fee, nonce, Vec::new())?;
    let result = rpc_call(
        &rpc,
        "cog_sendTransaction",
        json!({
            "pubkey": hex::encode(tx.pubkey),
            "to": tx.to.to_hex(),
            "amount": tx.amount,
            "fee": tx.fee,
            "nonce": tx.nonce,
            "memo": "",
            "signature": hex::encode(tx.signature),
        }),
    )
    .await?;
    println!(
        "sent {} COG on {chain_id} to {} (fee {} COG, nonce {})",
        format_cog(amount),
        to,
        format_cog(fee),
        nonce
    );
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            genesis,
            data_dir,
            rpc,
            p2p,
            peer,
        } => cmd_run(genesis, data_dir, rpc, p2p, peer).await,

        Command::Keygen { out } => {
            if out.exists() {
                bail!(
                    "{} already exists; refusing to overwrite a key",
                    out.display()
                );
            }
            let kp = Keypair::generate();
            std::fs::write(&out, serde_json::to_string_pretty(&kp)? + "\n")?;
            println!("wrote {}", out.display());
            println!("address: {}", kp.address);
            println!("KEEP THIS FILE SECRET AND BACKED UP.");
            Ok(())
        }

        Command::Address { key } => {
            let kp = load_key(&key)?;
            println!("{}", kp.address);
            Ok(())
        }

        Command::Balance { address, rpc } => {
            let result = rpc_call(&rpc, "cog_getBalance", json!({"address": address})).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }

        Command::Send {
            key,
            to,
            amount,
            fee,
            rpc,
        } => cmd_send(key, to, amount, fee, rpc).await,

        Command::GenesisTemplate {
            out,
            chain_id,
            founders,
            ecosystem,
            liquidity,
            initial_difficulty,
            block_time,
        } => cmd_genesis_template(
            out,
            chain_id,
            founders,
            ecosystem,
            liquidity,
            initial_difficulty,
            block_time,
        ),

        Command::WalletUi {
            key,
            bind,
            rpc,
            allow_remote,
            no_open,
        } => {
            let keypair = load_key(&key)?;
            println!("portfel: {}", keypair.address);
            wallet_ui::serve(
                wallet_ui::WalletContext {
                    keypair: Arc::new(keypair),
                    node: rpc,
                },
                bind,
                allow_remote,
                !no_open,
            )
            .await
        }

        Command::InspectGenesis { genesis } => cmd_inspect_genesis(genesis),

        Command::Selftest => cmd_selftest(),
    }
}
