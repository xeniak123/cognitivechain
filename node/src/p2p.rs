//! Minimal length-prefixed TCP gossip between validator nodes.
//!
//! Wire format: `u32` little-endian length, then a bincode-encoded [`Message`].
//! Every node both listens and dials its configured peers; duplicate links are
//! harmless because block acceptance is idempotent.

use crate::chain::{Accepted, Chain};
use crate::types::{Block, Hash};
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

/// Hard cap on a single frame (a block with a full reveal is ~200 KiB).
const MAX_FRAME: u32 = 8 * 1024 * 1024;
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    Hello {
        chain_id: String,
        genesis: Hash,
        height: u64,
    },
    NewBlock(Box<Block>),
    /// Ask for up to 128 canonical blocks starting at this height.
    GetBlocks {
        from_height: u64,
    },
    Blocks(Vec<Block>),
}

pub struct P2p {
    pub chain: Arc<Mutex<Chain>>,
    pub announce: broadcast::Sender<Block>,
}

async fn write_message(stream: &mut TcpStream, msg: &Message) -> Result<()> {
    let payload = bincode::serialize(msg)?;
    if payload.len() as u32 > MAX_FRAME {
        bail!(
            "outgoing frame of {} bytes exceeds the limit",
            payload.len()
        );
    }
    stream
        .write_all(&(payload.len() as u32).to_le_bytes())
        .await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_message(stream: &mut TcpStream) -> Result<Message> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME {
        bail!("peer announced an oversized frame of {len} bytes");
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).await?;
    Ok(bincode::deserialize(&payload)?)
}

impl P2p {
    fn hello(&self) -> Result<Message> {
        let chain = self.chain.lock();
        Ok(Message::Hello {
            chain_id: chain.params.chain_id.clone(),
            genesis: chain.genesis_hash()?,
            height: chain.tip.header.height,
        })
    }

    fn check_hello(&self, chain_id: &str, genesis: &Hash) -> Result<()> {
        let chain = self.chain.lock();
        if chain.params.chain_id != chain_id {
            bail!(
                "peer is on chain {chain_id:?}, we are on {:?}",
                chain.params.chain_id
            );
        }
        if chain.genesis_hash()? != *genesis {
            bail!("peer has a different genesis block");
        }
        Ok(())
    }

    /// Feed a block into the chain; returns true when it advanced our tip.
    fn ingest(&self, block: Block) -> Result<bool> {
        let mut chain = self.chain.lock();
        let outcome = chain.accept_block(block)?;
        match outcome {
            Accepted::Extended { height, .. } => {
                tracing::info!("adopted peer block at height {height}");
                Ok(true)
            }
            Accepted::Reorganised { height, depth, .. } => {
                tracing::warn!("reorganised to height {height} (depth {depth})");
                Ok(true)
            }
            Accepted::Duplicate | Accepted::SideBranch { .. } => Ok(false),
            Accepted::Orphan { missing_parent } => {
                tracing::debug!(
                    "orphan block, missing parent {}",
                    hex::encode(missing_parent)
                );
                Ok(false)
            }
        }
    }

    fn collect_blocks(&self, from_height: u64) -> Result<Vec<Block>> {
        let chain = self.chain.lock();
        let mut out = Vec::new();
        for h in from_height..=chain.tip.header.height {
            if out.len() >= 128 {
                break;
            }
            if let Some(b) = chain.block_at_height(h)? {
                out.push(b);
            }
        }
        Ok(out)
    }

    fn local_height(&self) -> u64 {
        self.chain.lock().tip.header.height
    }
}

/// Serve one peer connection: read its frames and push our announcements to it.
async fn run_peer(p2p: Arc<P2p>, mut stream: TcpStream) -> Result<()> {
    let peer = stream.peer_addr().ok();
    write_message(&mut stream, &p2p.hello()?).await?;

    let mut announcements = p2p.announce.subscribe();
    let mut handshaken = false;

    loop {
        tokio::select! {
            incoming = read_message(&mut stream) => {
                let msg = incoming?;
                match msg {
                    Message::Hello { chain_id, genesis, height } => {
                        p2p.check_hello(&chain_id, &genesis)?;
                        handshaken = true;
                        tracing::info!("peer {peer:?} handshake ok at height {height}");
                        let ours = p2p.local_height();
                        if height > ours {
                            write_message(
                                &mut stream,
                                &Message::GetBlocks { from_height: ours + 1 },
                            )
                            .await?;
                        }
                    }
                    Message::NewBlock(block) => {
                        if !handshaken {
                            bail!("peer sent a block before the handshake");
                        }
                        let height = block.header.height;
                        let advanced = p2p.ingest(*block)?;
                        if !advanced {
                            let ours = p2p.local_height();
                            if height > ours + 1 {
                                write_message(
                                    &mut stream,
                                    &Message::GetBlocks { from_height: ours + 1 },
                                )
                                .await?;
                            }
                        }
                    }
                    Message::GetBlocks { from_height } => {
                        let blocks = p2p.collect_blocks(from_height)?;
                        write_message(&mut stream, &Message::Blocks(blocks)).await?;
                    }
                    Message::Blocks(blocks) => {
                        if !handshaken {
                            bail!("peer sent blocks before the handshake");
                        }
                        let mut progressed = false;
                        for block in blocks {
                            match p2p.ingest(block) {
                                Ok(adv) => progressed |= adv,
                                Err(err) => {
                                    tracing::warn!("rejecting peer block: {err:#}");
                                    break;
                                }
                            }
                        }
                        if progressed {
                            let ours = p2p.local_height();
                            write_message(
                                &mut stream,
                                &Message::GetBlocks { from_height: ours + 1 },
                            )
                            .await?;
                        }
                    }
                }
            }
            announced = announcements.recv() => {
                match announced {
                    Ok(block) => {
                        write_message(&mut stream, &Message::NewBlock(Box::new(block))).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("announcement channel lagged by {n} blocks");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        bail!("announcement channel closed");
                    }
                }
            }
        }
    }
}

pub async fn listen(p2p: Arc<P2p>, addr: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind P2P listener on {addr}"))?;
    tracing::info!("P2P listening on {addr}");
    loop {
        let (stream, peer) = listener.accept().await?;
        let p2p = p2p.clone();
        tokio::spawn(async move {
            if let Err(err) = run_peer(p2p, stream).await {
                tracing::info!("peer {peer} disconnected: {err:#}");
            }
        });
    }
}

/// Keep an outbound connection to `addr` alive, retrying forever.
pub async fn dial_forever(p2p: Arc<P2p>, addr: String) {
    loop {
        match TcpStream::connect(&addr).await {
            Ok(stream) => {
                tracing::info!("connected to peer {addr}");
                if let Err(err) = run_peer(p2p.clone(), stream).await {
                    tracing::info!("peer {addr} dropped: {err:#}");
                }
            }
            Err(err) => {
                tracing::debug!("cannot reach peer {addr}: {err}");
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}
