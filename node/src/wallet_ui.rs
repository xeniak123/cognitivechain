//! Local graphical wallet.
//!
//! `cog-node wallet-ui` starts a small web server on loopback and serves a page
//! for checking a balance and sending COG.
//!
//! # Where the signing happens
//!
//! In this process, in Rust, using the same audited ed25519 implementation the
//! node validates with. The browser never receives the private key, never sees
//! the seed, and cannot ask for it: the page posts `{to, amount, fee}` and gets
//! back a transaction hash. That is the whole reason this is a local server
//! rather than a static page doing crypto in JavaScript.
//!
//! The server binds to loopback and refuses any other interface unless the
//! operator explicitly opts in, because anything that can reach this port can
//! spend the wallet.

use crate::client::rpc_call;
use crate::crypto::Keypair;
use crate::types::{format_cog, parse_cog, Address};
use anyhow::{bail, Context, Result};
use axum::extract::State as AxumState;
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Clone)]
pub struct WalletContext {
    pub keypair: Arc<Keypair>,
    /// Node JSON-RPC endpoint this wallet talks to.
    pub node: String,
}

/// Open the system browser at `url`. Best effort: a wallet that could not
/// launch a browser is still a working wallet, so failure is never fatal.
fn open_browser(url: &str) {
    let result = if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(err) = result {
        tracing::debug!("could not open a browser automatically: {err}");
    }
}

pub async fn serve(
    ctx: WalletContext,
    bind: SocketAddr,
    allow_remote: bool,
    open: bool,
) -> Result<()> {
    if !bind.ip().is_loopback() && !allow_remote {
        bail!(
            "refusing to bind the wallet to {bind}: anything that reaches this port can spend \
             your coins. Use a loopback address, or pass --allow-remote if you really mean it \
             and have your own authentication in front."
        );
    }
    if !bind.ip().is_loopback() {
        tracing::warn!(
            "wallet UI is listening on {bind}, which is NOT loopback. Anyone who can reach \
             this port can send your COG. Put authentication in front of it."
        );
    }

    let app = Router::new()
        .route("/", get(page))
        .route("/api/state", get(state))
        .route("/api/send", post(send))
        .with_state(ctx);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("cannot bind wallet UI on {bind}"))?;
    let url = format!("http://{bind}");
    println!();
    println!("  Portfel CognitiveChain");
    println!("  {url}");
    println!();
    if open {
        println!("  Otwieram przeglądarkę...");
        open_browser(&url);
    } else {
        println!("  Otwórz ten adres w przeglądarce.");
    }
    println!("  Zatrzymanie: Ctrl+C");
    println!();
    axum::serve(listener, app).await?;
    Ok(())
}

async fn page() -> Html<&'static str> {
    Html(include_str!("wallet.html"))
}

fn fail(status: StatusCode, message: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": message.to_string() })))
}

async fn state(AxumState(ctx): AxumState<WalletContext>) -> (StatusCode, Json<Value>) {
    let address = match ctx.keypair.address() {
        Ok(a) => a,
        Err(err) => return fail(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let history = rpc_call(
        &ctx.node,
        "cog_getAddressHistory",
        json!({ "address": address.to_hex(), "limit": 50 }),
    )
    .await;

    match history {
        Ok(mut result) => {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("node".into(), json!(ctx.node));
            }
            (StatusCode::OK, Json(result))
        }
        Err(err) => fail(StatusCode::BAD_GATEWAY, err),
    }
}

async fn send(
    AxumState(ctx): AxumState<WalletContext>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let to_raw = body.get("to").and_then(|v| v.as_str()).unwrap_or("").trim();
    let amount_raw = body.get("amount").and_then(|v| v.as_str()).unwrap_or("");
    let fee_raw = body.get("fee").and_then(|v| v.as_str()).unwrap_or("0.001");

    let to = match Address::parse(to_raw) {
        Ok(a) => a,
        Err(err) => return fail(StatusCode::BAD_REQUEST, format!("adres odbiorcy: {err}")),
    };
    let amount = match parse_cog(amount_raw) {
        Ok(v) => v,
        Err(err) => return fail(StatusCode::BAD_REQUEST, format!("kwota: {err}")),
    };
    let fee = match parse_cog(fee_raw) {
        Ok(v) => v,
        Err(err) => return fail(StatusCode::BAD_REQUEST, format!("opłata: {err}")),
    };
    if amount == 0 {
        return fail(StatusCode::BAD_REQUEST, "kwota musi być większa od zera");
    }

    let address = match ctx.keypair.address() {
        Ok(a) => a,
        Err(err) => return fail(StatusCode::INTERNAL_SERVER_ERROR, err),
    };
    if to == address {
        return fail(StatusCode::BAD_REQUEST, "nie można wysłać do samego siebie");
    }

    // Always take the nonce from the node rather than trusting the page: a stale
    // browser tab would otherwise sign a transaction that can never be included.
    let account = match rpc_call(
        &ctx.node,
        "cog_getBalance",
        json!({ "address": address.to_hex() }),
    )
    .await
    {
        Ok(v) => v,
        Err(err) => return fail(StatusCode::BAD_GATEWAY, err),
    };
    let nonce = account.get("nonce").and_then(|v| v.as_u64()).unwrap_or(0);

    // The chain id is part of what gets signed, so it has to come from the node
    // this wallet is actually talking to.
    let chain_id = match rpc_call(&ctx.node, "cog_status", json!({})).await {
        Ok(v) => v
            .get("chain_id")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string(),
        Err(err) => return fail(StatusCode::BAD_GATEWAY, err),
    };
    if chain_id.is_empty() {
        return fail(StatusCode::BAD_GATEWAY, "węzeł nie podał chain_id");
    }
    let balance: u64 = account
        .get("balance_acog")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let total = match amount.checked_add(fee) {
        Some(v) => v,
        None => return fail(StatusCode::BAD_REQUEST, "kwota z opłatą przekracza zakres"),
    };
    if total > balance {
        return fail(
            StatusCode::BAD_REQUEST,
            format!(
                "brak środków: masz {} COG, potrzeba {} COG",
                format_cog(balance),
                format_cog(total)
            ),
        );
    }

    let tx = match ctx
        .keypair
        .sign_transfer(&chain_id, to, amount, fee, nonce, Vec::new())
    {
        Ok(tx) => tx,
        Err(err) => return fail(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let result = rpc_call(
        &ctx.node,
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
    .await;

    match result {
        Ok(v) => (
            StatusCode::OK,
            Json(json!({
                "tx_hash": v.get("tx_hash").cloned().unwrap_or(Value::Null),
                "amount_cog": format_cog(amount),
                "fee_cog": format_cog(fee),
                "to": to.to_hex(),
                "nonce": nonce,
            })),
        ),
        Err(err) => fail(StatusCode::BAD_GATEWAY, err),
    }
}
