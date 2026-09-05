//! Minimal JSON-RPC client over plain HTTP/1.1.
//!
//! Deliberately dependency-free: the wallet and the CLI talk to a node without
//! pulling in an HTTP stack, and a wallet with a small dependency tree is a
//! wallet with a small supply-chain surface.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Normalise `1.2.3.4`, `1.2.3.4:26657` or `http://host:port` to `host:port`.
pub fn normalise_endpoint(endpoint: &str) -> String {
    let host = endpoint
        .trim()
        .trim_start_matches("http://")
        .trim_end_matches('/');
    if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:26657")
    }
}

/// Issue one JSON-RPC call and return its `result`.
pub async fn rpc_call(endpoint: &str, method: &str, params: Value) -> Result<Value> {
    let host = normalise_endpoint(endpoint);
    let body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))?;
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    let mut stream = tokio::net::TcpStream::connect(&host)
        .await
        .with_context(|| format!("cannot reach node RPC at {host}"))?;
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let text = String::from_utf8_lossy(&response).to_string();
    let split = text
        .find("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response from node"))?;
    let body_text = &text[split + 4..];

    // Slice out the JSON object, which makes the client tolerant of a chunked
    // transfer encoding without decoding the framing.
    let start = body_text
        .find('{')
        .ok_or_else(|| anyhow!("node response contains no JSON body"))?;
    let end = body_text
        .rfind('}')
        .ok_or_else(|| anyhow!("node response contains a truncated JSON body"))?;
    let parsed: Value = serde_json::from_str(&body_text[start..=end])
        .with_context(|| format!("node returned a non-JSON body: {}", &body_text[start..=end]))?;

    if let Some(err) = parsed.get("error") {
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        bail!("{message}");
    }
    parsed
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("response has no result field"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_forms() {
        assert_eq!(normalise_endpoint("1.2.3.4"), "1.2.3.4:26657");
        assert_eq!(normalise_endpoint("1.2.3.4:9999"), "1.2.3.4:9999");
        assert_eq!(
            normalise_endpoint("http://node.example:26657/"),
            "node.example:26657"
        );
        assert_eq!(normalise_endpoint("  localhost  "), "localhost:26657");
    }
}
