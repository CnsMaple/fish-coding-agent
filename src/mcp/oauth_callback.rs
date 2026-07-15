//! Local HTTP server that catches the OAuth redirect for remote
//! MCP servers that need user authorization. Mirrors opencode's
//! `packages/opencode/src/mcp/oauth-callback.ts`.
//!
//! Runs a single `tokio::TcpListener` on `127.0.0.1`,
//! picks a port in the [19876, 19881] range if the default is
//! occupied, serves a minimal HTML page after the OAuth redirect
//! completes, and returns the authorization code.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tracing::info;

pub const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 19876;
pub const OAUTH_CALLBACK_PATH: &str = "/mcp/oauth/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// Run the callback server on a free port, wait for a matching
/// OAuth redirect, and return the authorization code.
///
/// The caller provides the expected `state` value so we can reject
/// mismatched CSRF tokens.
pub async fn wait_for_callback(expected_state: &str) -> Result<String, String> {
    let port = pick_free_port()
        .await
        .ok_or_else(|| "no free port available in the callback range".to_string())?;
    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("bind callback server: {e}"))?;
    info!(port, "MCP OAuth callback server listening");

    let expected = expected_state.to_owned();
    let started = std::time::Instant::now();

    while started.elapsed() < CALLBACK_TIMEOUT {
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Accept with a small timeout so we can check the overall timeout.
        let accept = tokio::time::timeout(Duration::from_millis(500), listener.accept()).await;
        let Ok(Ok((mut stream, _))) = accept else {
            continue;
        };

        let mut buf_reader = BufReader::new(&mut stream);
        let mut request_line = String::new();
        if buf_reader.read_line(&mut request_line).await.is_err() {
            continue;
        }

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 || parts[0] != "GET" {
            continue;
        }
        let path = parts[1];

        // Drain headers
        let mut header = String::new();
        loop {
            header.clear();
            if buf_reader.read_line(&mut header).await.is_err() {
                break;
            }
            if header.trim().is_empty() {
                break;
            }
        }

        // Parse query params from the path
        if let Some(query) = path.split('?').nth(1) {
            let params = parse_query(query);

            if let Some(code) = params
                .iter()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.clone())
            {
                let state_val = params
                    .iter()
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                if state_val == expected {
                    let _ = send_html_response(&mut stream, 200).await;
                    return Ok(code);
                }
                // State mismatch — keep waiting.
                let _ = send_html_response(&mut stream, 400).await;
                continue;
            }
        }

        // Unknown path — show help page
        let callback_url = format!("http://127.0.0.1:{port}{OAUTH_CALLBACK_PATH}");
        let body = format!(
            r#"<html><body><h1>MCP OAuth Callback</h1>
            <p>Waiting for authorization redirect from the MCP server.</p>
            <p>If you see this page, the callback URL is: <code>{callback_url}</code></p>
            <p>Expected state token is: <code>{expected}</code></p>
            </body></html>"#,
        );
        let _ = send_html_response_raw(&mut stream, 200, &body).await;
    }

    Err("OAuth callback timed out after 5 minutes".to_string())
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter_map(|pair| {
            let mut kv = pair.splitn(2, '=');
            let k = kv.next()?.to_string();
            let v = kv.next().unwrap_or("").to_string();
            Some((k, urlencoding_decode(&v)))
        })
        .collect()
}

/// Minimal URL-decoding: replaces `%XX` with the decoded byte.
fn urlencoding_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices();
    while let Some((_, c)) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).map(|(_, c)| c).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                out.push(byte as char);
            } else {
                out.push('%');
                out.push_str(&hex);
            }
        } else {
            out.push(c);
        }
    }
    out
}

async fn send_html_response(stream: &mut tokio::net::TcpStream, status: u16) -> String {
    let body = match status {
        200 => "Authorization complete! You can close this tab.",
        400 => "CSRF error: state mismatch. Check your auth URL.",
        _ => "Unknown error.",
    };
    send_html_response_raw(stream, status, body).await
}

async fn send_html_response_raw(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
) -> String {
    let status_line = match status {
        200 => "200 OK",
        400 => "400 Bad Request",
        _ => "500 Internal Server Error",
    };
    let content = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(content.as_bytes()).await;
    body.to_string()
}

/// Find a free port in [DEFAULT_OAUTH_CALLBACK_PORT, DEFAULT_OAUTH_CALLBACK_PORT+5].
async fn pick_free_port() -> Option<u16> {
    for port in DEFAULT_OAUTH_CALLBACK_PORT..=DEFAULT_OAUTH_CALLBACK_PORT + 5 {
        let addr = format!("127.0.0.1:{port}");
        if let Ok(listener) = TcpListener::bind(&addr).await {
            drop(listener);
            return Some(port);
        }
    }
    None
}
