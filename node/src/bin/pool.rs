//! `cog-pool`: a mining pool for CognitiveChain.
//!
//! Miners point `cog-miner --pool` at this process instead of at a node. It
//! hands them work under its own address, verifies every share by recomputing
//! part of the matrix product, forwards winning shares to a node, and pays out
//! proportionally to recent verified work.

use anyhow::{bail, Context, Result};
use clap::Parser;
use cog_node::crypto::Keypair;
use cog_node::pool::{self, Pool, PoolConfig};
use cog_node::types::parse_cog;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "cog-pool", version, about = "CognitiveChain mining pool")]
struct Cli {
    /// Keyfile of the pool wallet. It receives block rewards and signs payouts,
    /// so it must hold enough balance to cover transaction fees.
    #[arg(long, default_value = "pool-wallet.json")]
    key: PathBuf,
    /// Node this pool submits blocks to.
    #[arg(long, default_value = "127.0.0.1:26657")]
    node: String,
    /// Address miners connect to.
    #[arg(long, default_value = "0.0.0.0:26659")]
    bind: SocketAddr,
    /// Percentage of each block the operator keeps.
    #[arg(long, default_value_t = 1.0)]
    fee_percent: f64,
    /// Difficulty a share must meet. Aim for a few shares per miner per minute:
    /// too high and small miners look idle, too low and the pool drowns in
    /// verification work.
    #[arg(long, default_value_t = 50_000)]
    share_difficulty: u64,
    /// Number of recent verified shares that split each block reward.
    #[arg(long, default_value_t = 10_000)]
    pplns_window: usize,
    /// Minimum credit before a payout is sent, in COG.
    #[arg(long, default_value = "1.0")]
    min_payout: String,
    /// Fee attached to payout transactions, in COG. Deducted from the payout.
    #[arg(long, default_value = "0.001")]
    payout_fee: String,
    /// Seconds between payout rounds.
    #[arg(long, default_value_t = 120)]
    payout_interval: u64,
    /// Where the ledger of what each miner is owed is kept.
    #[arg(long, default_value = "pool-ledger.json")]
    state: PathBuf,
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
    if !(0.0..=100.0).contains(&cli.fee_percent) {
        bail!("--fee-percent must be between 0 and 100");
    }
    if cli.share_difficulty == 0 {
        bail!("--share-difficulty must be greater than zero");
    }
    if cli.pplns_window == 0 {
        bail!("--pplns-window must be greater than zero");
    }

    let text = std::fs::read_to_string(&cli.key)
        .with_context(|| format!("cannot read pool keyfile {}", cli.key.display()))?;
    let stored: Keypair = serde_json::from_str(&text)
        .with_context(|| format!("cannot parse pool keyfile {}", cli.key.display()))?;
    let raw = hex::decode(&stored.secret)?;
    if raw.len() != 32 {
        bail!("pool secret key must be 32 bytes");
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&raw);
    let keypair = Keypair::from_seed(&seed);

    let config = PoolConfig {
        node: cli.node.clone(),
        bind: cli.bind,
        fee_percent: cli.fee_percent,
        share_difficulty: cli.share_difficulty,
        pplns_window: cli.pplns_window,
        min_payout: parse_cog(&cli.min_payout).map_err(|e| anyhow::anyhow!(e))?,
        payout_fee: parse_cog(&cli.payout_fee).map_err(|e| anyhow::anyhow!(e))?,
        state_path: cli.state.clone(),
    };

    let pool = Pool::new(config, keypair)?;

    println!();
    println!("  Pula CognitiveChain");
    println!("  adres puli        {}", pool.address);
    println!("  wezel             {}", cli.node);
    println!("  prowizja          {} %", cli.fee_percent);
    println!("  trudnosc udzialu  {}", cli.share_difficulty);
    println!("  okno PPLNS        {} udzialow", cli.pplns_window);
    println!("  ksiega            {}", cli.state.display());
    println!();
    println!("  Gornicy laczą sie komenda:");
    println!("    cog-miner --wallet <ICH_ADRES> --pool {}", cli.bind);
    println!();

    let follower = tokio::spawn(pool::follow_node(pool.clone()));
    let accountant = tokio::spawn(pool::account_rewards(pool.clone()));
    let payer = tokio::spawn(pool::pay_miners(
        pool.clone(),
        Duration::from_secs(cli.payout_interval),
    ));
    let server = tokio::spawn(pool::serve(pool));

    tokio::signal::ctrl_c().await.ok();
    println!("zatrzymuje pule");
    follower.abort();
    accountant.abort();
    payer.abort();
    server.abort();
    Ok(())
}
