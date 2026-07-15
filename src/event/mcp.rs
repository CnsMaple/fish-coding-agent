use super::AppMsg;
use crate::app::App;
use crate::function::CancelState;
use anyhow::Result;
/// Run the full OAuth authorization flow for a remote MCP server:
///
/// 1. Start a local TCP callback server
/// 2. Generate PKCE challenge
/// 3. Build the authorization URL (using stored `client_id` or
///    performing dynamic client registration)
/// 4. Open the browser
/// 5. Wait for the callback (5 min timeout)
/// 6. Exchange code for tokens
/// 7. Store tokens in `McpAuthStore`
/// 8. Reconnect the server with the new token
pub(super) async fn run_mcp_oauth(
    server_name: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<AppMsg>,
) -> Result<(), String> {
    use base64::Engine;
    use sha2::Digest;

    let Some(svc) = crate::mcp::McpRegistry::current() else {
        return Err(crate::commands::MSG_MCP_NOT_INIT.into());
    };

    // 1. Get server config. Must be a remote server.
    let config = svc.snapshot().await;
    let cfg = config
        .config
        .get(server_name)
        .ok_or_else(|| format!("server `{server_name}` not configured"))?;
    let (server_url, oauth_cfg) = match cfg {
        crate::mcp::McpServerConfig::Remote { url, oauth, .. } => {
            let oauth = oauth
                .as_ref()
                .ok_or_else(|| format!("server `{server_name}` has no OAuth config"))?;
            (url.clone(), oauth)
        }
        _ => return Err(format!("server `{server_name}` is not a remote server")),
    };

    // 2. Discover OAuth metadata from the MCP server.
    let well_known_url = format!(
        "{}/.well-known/oauth-authorization-server",
        server_url.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;
    let metadata: serde_json::Value = client
        .get(&well_known_url)
        .send()
        .await
        .map_err(|e| format!("fetch OAuth metadata: {e}"))?
        .json()
        .await
        .map_err(|e| format!("parse OAuth metadata: {e}"))?;

    let auth_url_str = metadata["authorization_endpoint"]
        .as_str()
        .ok_or_else(|| "no authorization_endpoint in OAuth metadata".to_string())?;
    let token_url_str = metadata["token_endpoint"]
        .as_str()
        .ok_or_else(|| "no token_endpoint in OAuth metadata".to_string())?;

    // 3. Generate PKCE challenge (S256).
    //    Code verifier: uuid-based random token.
    let code_verifier = uuid::Uuid::new_v4().to_string()
        + &uuid::Uuid::new_v4().to_string()
        + &uuid::Uuid::new_v4().to_string();
    // The verifier must be 43-128 chars as per RFC 7636. Uuid hex is 36
    // chars each, so three give us 108. Replace dashes to get only
    // unreserved chars.
    let code_verifier: String = code_verifier.chars().filter(|c| *c != '-').collect();
    let code_challenge_hash = sha2::Sha256::digest(code_verifier.as_bytes());
    let code_challenge =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(code_challenge_hash);

    // 4. Build the redirect URI.
    let port = crate::mcp::oauth_callback::DEFAULT_OAUTH_CALLBACK_PORT;
    let redirect_uri = oauth_cfg.redirect_uri.clone().unwrap_or_else(|| {
        format!(
            "http://127.0.0.1:{port}{}",
            crate::mcp::oauth_callback::OAUTH_CALLBACK_PATH
        )
    });

    // 5. Determine client_id: use configured value or try dynamic
    //    client registration.
    let client_id = if let Some(cid) = &oauth_cfg.client_id {
        cid.clone()
    } else {
        // TODO: dynamic client registration (POST to registration endpoint)
        // For now, require configured client_id.
        return Err("no client_id configured; add `oauth.client_id` to your config".into());
    };

    // 6. Generate a random state token for CSRF protection.
    let state_token = uuid::Uuid::new_v4().to_string();

    // 7. Start the callback server and wait for the redirect.
    //    Clone the state token so the spawned task can own it.
    let state_for_callback = state_token.clone();
    let callback_handle = tokio::spawn(async move {
        crate::mcp::oauth_callback::wait_for_callback(&state_for_callback).await
    });

    // 8. Build and open the authorization URL.
    use url::form_urlencoded;
    let mut query_parts: Vec<(&str, &str)> = vec![
        ("response_type", "code"),
        ("client_id", &client_id),
        ("redirect_uri", &redirect_uri),
        ("state", &state_token),
        ("code_challenge", &code_challenge),
        ("code_challenge_method", "S256"),
    ];
    if let Some(scope) = &oauth_cfg.scope {
        query_parts.push(("scope", scope));
    }
    let auth_url_str = format!(
        "{}?{}",
        auth_url_str,
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(query_parts)
            .finish()
    );

    tracing::info!(
        server = %server_name,
        url = %auth_url_str,
        "opening browser for MCP OAuth"
    );
    let _ = tx.send(AppMsg::McpAuthRequired {
        server: server_name.to_string(),
        url: auth_url_str.clone(),
        error: String::new(),
    });
    // Best-effort browser open.
    if let Err(e) = open::that(&auth_url_str) {
        let _ = tx.send(AppMsg::McpBrowserOpenFailed {
            server: server_name.to_string(),
            url: auth_url_str,
        });
        return Err(format!("open browser: {e}. URL shown in toast above."));
    }

    // 9. Wait for the callback result (or timeout).
    let cb = callback_handle
        .await
        .map_err(|e| format!("callback task failed: {e}"))?
        .map_err(|e| format!("callback error: {e}"))?;

    // 10. Exchange the auth code for tokens.
    let token_params = [
        ("grant_type", "authorization_code"),
        ("code", &cb),
        ("redirect_uri", &redirect_uri),
        ("client_id", &client_id),
        ("code_verifier", &code_verifier),
    ];
    let token_resp: serde_json::Value = client
        .post(token_url_str)
        .form(&token_params)
        .send()
        .await
        .map_err(|e| format!("token exchange request: {e}"))?
        .json()
        .await
        .map_err(|e| format!("token exchange parse: {e}"))?;

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| "no access_token in token response".to_string())?
        .to_string();
    let refresh_token = token_resp["refresh_token"].as_str().map(|s| s.to_string());
    let expires_in = token_resp["expires_in"].as_i64();
    let expires_at = expires_in.map(|secs| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            + secs
    });

    // 11. Store tokens.
    let store = crate::mcp::auth::McpAuthStore::load_or_default();
    store.set(
        server_name,
        crate::mcp::auth::Entry {
            tokens: Some(crate::mcp::auth::Tokens {
                access_token,
                refresh_token,
                expires_at,
                scope: oauth_cfg.scope.clone(),
            }),
            client_info: None,
            server_url: Some(server_url.clone()),
        },
    );

    tracing::info!(server = %server_name, "OAuth tokens stored, reconnecting...");

    // 12. Reconnect the server.
    svc.connect(server_name, cfg).await;
    Ok(())
}
pub(super) fn submit_direct_tool_input(app: &mut App, raw: &str) -> bool {
    let (name, title, args, include_context) = if let Some(code) = raw.strip_prefix("!!") {
        let command = code.trim_start().to_string();
        if command.trim().is_empty() {
            app.notify(
                crate::function::notifications::ToastLevel::Fail,
                "shell command is empty",
            );
            return true;
        }
        (
            "shell_command".to_string(),
            format!("$ {}", command.trim()),
            serde_json::json!({ "command": command }).to_string(),
            true,
        )
    } else if let Some(code) = raw.strip_prefix('!') {
        let command = code.trim_start().to_string();
        if command.trim().is_empty() {
            app.notify(
                crate::function::notifications::ToastLevel::Fail,
                "shell command is empty",
            );
            return true;
        }
        (
            "shell_command".to_string(),
            format!("$ {}", command.trim()),
            serde_json::json!({ "command": command }).to_string(),
            false,
        )
    } else if let Some(code) = raw.strip_prefix("$$") {
        let code = code.trim_start().to_string();
        if code.trim().is_empty() {
            app.notify(
                crate::function::notifications::ToastLevel::Fail,
                "python code is empty",
            );
            return true;
        }
        (
            "python_command".to_string(),
            "python".to_string(),
            serde_json::json!({ "code": code }).to_string(),
            true,
        )
    } else if let Some(code) = raw.strip_prefix('$') {
        let code = code.trim_start().to_string();
        if code.trim().is_empty() {
            app.notify(
                crate::function::notifications::ToastLevel::Fail,
                "python code is empty",
            );
            return true;
        }
        (
            "python_command".to_string(),
            "python".to_string(),
            serde_json::json!({ "code": code }).to_string(),
            false,
        )
    } else {
        return false;
    };

    use crate::session::Message;
    app.maybe_title_from_first_prompt(raw);
    app.session
        .push(Message::new(crate::session::Role::User, raw.to_string()));

    // Create an empty streaming assistant message for tool output
    let assistant = Message {
        role: crate::session::Role::Assistant,
        content: String::new(),
        thinking: String::new(),
        thinking_segments: Vec::new(),
        thinking_visible: false,
        tool_results: Vec::new(),
        tool_calls: Vec::new(),
        attachments: Vec::new(),
        display_cursor: 0,
        line_count: 0,
        cached_content_line_count: None,
        ts: chrono::Utc::now(),
        streaming: true,
        skill_ref: None,
        content_version: 0,
    };
    let id = app.session.push(assistant);
    app.session.streaming_id = Some(id);

    if let Some(tx) = app.msg_tx.clone() {
        let cwd = app.cwd.clone();
        let n = name.clone();
        let t = title.clone();
        // Set up an inflight handle so the spinner / pending tool
        // block paints immediately, and so Esc can later cancel or
        // drop the request. The actual `tokio::spawn` is deferred
        // until after the next `terminal.draw(...)` returns (see
        // `flush_pending_request` in the main event loop) so the
        // user message and pending tool block are on screen first.
        app.current_request_seq = app.current_request_seq.wrapping_add(1);
        let seq = app.current_request_seq;
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        app.inflight = Some(crate::app::InflightHandle {
            cancel: cancel_tx,
            label: format!("tool:{n}"),
            seq,
            started_at: std::time::Instant::now(),
        });
        app.cancel_state = CancelState::Idle;
        app.pending_request = Some(crate::function::PendingRequest::Tool(
            crate::function::ToolPending {
                name: n,
                title: t,
                args,
                include_context,
                cwd,
                cancel_rx,
                tx,
                seq,
            },
        ));
    } else {
        app.notify(
            crate::function::notifications::ToastLevel::Fail,
            "event channel is not available",
        );
    }
    true
}
/// Body of the direct-tool-input spawn. Extracted from
/// `submit_direct_tool_input` so the same body can be invoked from
/// `flush_pending_request` after the user message has been rendered.
#[allow(clippy::too_many_arguments)]
pub async fn run_tool_execution(
    name: String,
    title: String,
    args: String,
    include_context: bool,
    cwd: std::path::PathBuf,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    tx: tokio::sync::mpsc::UnboundedSender<AppMsg>,
    seq: u64,
) {
    // Helper that mirrors `run_chat_stream`'s: only deliver messages
    // when the user hasn't cancelled. Tool requests have the same
    // stale-event problem as chat — a tool invoked before Esc must
    // not push a `ChatDone` after the next request has started.
    let send_msg = |msg: AppMsg| {
        if !*cancel_rx.borrow() {
            let _ = tx.send(msg);
        }
    };
    if *cancel_rx.borrow() {
        // User cancelled between submit and the deferred spawn.
        // Silent exit; if a follow-up request is already armed it
        // owns `current_request_seq` and will not be disturbed.
        return;
    }
    send_msg(AppMsg::ToolStarted {
        call_id: String::new(),
        name: name.clone(),
        title: title.clone(),
    });
    let result = crate::tools::execute_tool_streaming(&name, &args, &cwd, "", tx.clone()).await;
    if *cancel_rx.borrow() {
        return;
    }
    let display = tool_result_display(&result);
    let failed = tool_result_failed(&result);
    let metadata = crate::tools::extract_metadata(&result);
    let context = if include_context {
        Some(local_tool_context(&name, &title, &display))
    } else {
        None
    };
    send_msg(AppMsg::ChatToolResult {
        name,
        title,
        content: display,
        metadata,
        call_id: String::new(),
        failed,
    });
    send_msg(AppMsg::ChatDone { seq });
    if let Some(ctx) = context {
        send_msg(AppMsg::ChatDebug(ctx));
    }
}
pub(super) fn tool_result_display(result: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(result) {
        if val.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            val.get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        } else {
            val.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or(result)
                .to_string()
        }
    } else {
        result.to_string()
    }
}
pub(super) fn tool_result_failed(result: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(result)
        .ok()
        .and_then(|v| v.get("ok").and_then(|o| o.as_bool()))
        .map(|ok| !ok)
        .unwrap_or(false)
}
pub(super) fn local_tool_context(name: &str, title: &str, content: &str) -> String {
    format!(
        "Context from {name}:
{title}

{content}"
    )
}
pub(super) fn start_cursor_oauth(app: &mut App) {
    use crate::function::notifications::ToastLevel;
    let params = crate::providers::cursor::generate_auth_params();
    let login_url = params.login_url.clone();
    match crate::providers::cursor::open_browser(&login_url) {
        Ok(_) => app.notify(ToastLevel::Info, "opened Cursor OAuth login in browser"),
        Err(e) => app.notify(
            ToastLevel::Warn,
            format!("open browser failed: {e}; visit {login_url}"),
        ),
    }
    let client = app.reqwest.clone();
    if let Some(tx) = app.msg_tx.clone() {
        tokio::spawn(async move {
            match crate::providers::cursor::poll_auth(&client, &params.uuid, &params.verifier).await
            {
                Ok(tokens) => {
                    let _ = tx.send(AppMsg::CursorAuthSucceeded {
                        access_token: tokens.access_token,
                        refresh_token: tokens.refresh_token,
                    });
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::CursorAuthFailed(format!("{e}")));
                }
            }
        });
    }
}
