use super::super::ProviderError;
use anyhow::Result;
use base64::Engine;
use reqwest::StatusCode;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const CURSOR_LOGIN_URL: &str = "https://cursor.com/loginDeepControl";
const CURSOR_POLL_URL: &str = "https://api2.cursor.sh/auth/poll";
const CURSOR_REFRESH_URL: &str = "https://api2.cursor.sh/auth/exchange_user_api_key";

#[derive(Debug, Clone)]
pub struct CursorAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone)]
pub struct CursorAuthParams {
    pub verifier: String,
    pub uuid: String,
    pub login_url: String,
}

pub fn generate_auth_params() -> CursorAuthParams {
    let mut seed = Vec::new();
    seed.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    seed.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    seed.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(seed);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    let uuid = uuid::Uuid::new_v4().to_string();
    let login_url = format!(
        "{CURSOR_LOGIN_URL}?challenge={challenge}&uuid={uuid}&mode=login&redirectTarget=cli"
    );
    CursorAuthParams {
        verifier,
        uuid,
        login_url,
    }
}

pub fn open_browser(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};

    #[cfg(target_os = "windows")]
    {
        // Avoid `cmd /C start`: Cursor OAuth URLs contain `&`, which cmd treats
        // as command separators unless every layer quotes perfectly. rundll32
        // receives the URL directly and keeps accidental shell output out of
        // the TUI.
        Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    Ok(())
}

pub async fn poll_auth(
    client: &reqwest::Client,
    uuid: &str,
    verifier: &str,
) -> Result<CursorAuthTokens> {
    let mut delay_ms: u64 = 1000;
    let mut consecutive_errors = 0;
    for _ in 0..150 {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        let url = format!("{CURSOR_POLL_URL}?uuid={uuid}&verifier={verifier}");
        match client.get(&url).send().await {
            Ok(resp) if resp.status() == StatusCode::NOT_FOUND => {
                consecutive_errors = 0;
                delay_ms = ((delay_ms as f64 * 1.2) as u64).min(10_000);
            }
            Ok(resp) if resp.status().is_success() => {
                let body: CursorAuthResp = resp.json().await.map_err(ProviderError::Http)?;
                return Ok(CursorAuthTokens {
                    access_token: body.access_token,
                    refresh_token: body.refresh_token,
                });
            }
            Ok(resp) => {
                return Err(ProviderError::Other(format!(
                    "Cursor auth poll status {}",
                    resp.status()
                ))
                .into())
            }
            Err(_) => {
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    return Err(ProviderError::Other(
                        "too many Cursor auth polling errors".to_string(),
                    )
                    .into());
                }
            }
        }
    }
    Err(ProviderError::Other("Cursor authentication polling timeout".to_string()).into())
}

pub async fn refresh_token(client: &reqwest::Client, refresh: &str) -> Result<CursorAuthTokens> {
    let resp = client
        .post(CURSOR_REFRESH_URL)
        .bearer_auth(refresh)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .map_err(ProviderError::Http)?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(
            ProviderError::Other(format!("Cursor token refresh status {status}: {text}")).into(),
        );
    }
    let body: CursorAuthResp = resp.json().await.map_err(ProviderError::Http)?;
    Ok(CursorAuthTokens {
        access_token: body.access_token,
        refresh_token: body.refresh_token,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorAuthResp {
    access_token: String,
    refresh_token: String,
}
