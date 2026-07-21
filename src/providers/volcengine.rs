use super::{ChatEvent, ChatRequest, Provider, ProviderError};
use crate::config::ProviderKind;
use crate::function::notifications::ModelInfo;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

const VOLCENGINE_API_HOST: &str = "open.volcengineapi.com";
const VOLCENGINE_REGION: &str = "cn-beijing";
const VOLCENGINE_SERVICE: &str = "ark";
const VOLCENGINE_API_VERSION: &str = "2024-01-01";

/// Block size of SHA-256 (64 bytes).
const BLOCK_SIZE: usize = 64;

pub struct VolcengineProvider;

#[async_trait]
impl Provider for VolcengineProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Volcengine
    }

    async fn list_models(
        &self,
        client: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        access_key: &str,
        secret_key: &str,
    ) -> Result<Vec<ModelInfo>> {
        // Try OpenAI-compatible /api/v3/models endpoint (uses Bearer token)
        if !api_key.is_empty() {
            if let Ok(models) = self
                .list_via_openai_endpoint(client, base_url, api_key)
                .await
            {
                if !models.is_empty() {
                    return Ok(models);
                }
            }
        }

        // Try ListEndpoints via management API (returns user's deployed endpoints)
        if !access_key.is_empty() && !secret_key.is_empty() {
            if let Ok(models) = self.list_endpoints(client, access_key, secret_key).await {
                if !models.is_empty() {
                    return Ok(models);
                }
            }
        }

        // Fallback: ListFoundationModels via management API with AK/SK
        if !api_key.is_empty() || (!access_key.is_empty() && !secret_key.is_empty()) {
            if let Ok(models) = self
                .list_via_agent_plan_api(client, api_key, access_key, secret_key)
                .await
            {
                if !models.is_empty() {
                    return Ok(models);
                }
            }
        }

        self.list_via_management_api(client, access_key, secret_key)
            .await
    }

    async fn chat_stream(
        &self,
        client: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        req: ChatRequest,
        tx: mpsc::UnboundedSender<ChatEvent>,
    ) -> Result<()> {
        super::openai::OpenAiProvider
            .chat_stream(client, base_url, api_key, req, tx)
            .await
    }
}

/// Agent Plan API host and service for ListArkAgentPlanModel
const VOLCENGINE_AGENT_PLAN_HOST: &str = "open.volcengineapi.com";
const VOLCENGINE_AGENT_PLAN_SERVICE: &str = "ark";

impl VolcengineProvider {
    async fn list_via_openai_endpoint(
        &self,
        client: &reqwest::Client,
        base_url: &str,
        api_key: &str,
    ) -> Result<Vec<ModelInfo>> {
        let host = base_url
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or("ark.cn-beijing.volces.com");
        let models_url = format!("https://{host}/api/v3/models");
        let resp = client
            .get(&models_url)
            .bearer_auth(api_key)
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(ProviderError::Http)?;

        let status = resp.status();
        if !status.is_success() {
            return Err(ProviderError::Other(format!(
                "OpenAI models endpoint returned status {status}",
            ))
            .into());
        }

        let text = resp.text().await.map_err(ProviderError::Http)?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(ProviderError::Json)?;

        let mut models = Vec::new();
        if let Some(data) = v.get("data").and_then(|d| d.as_array()) {
            for item in data {
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if id.is_empty() {
                    continue;
                }
                models.push(ModelInfo {
                    id: id.to_string(),
                    display: id.to_string(),
                    request_id: None,
                    context_window_tokens: None,
                    context_needs_pick: false,
                    modalities: Vec::new(),
                });
            }
        }

        Ok(models)
    }

    async fn list_endpoints(
        &self,
        client: &reqwest::Client,
        access_key: &str,
        secret_key: &str,
    ) -> Result<Vec<ModelInfo>> {
        let query = format!("Action=ListEndpoints&Version={}", VOLCENGINE_API_VERSION);
        let body = b"{}";
        let resp = self
            .signed_post(client, &query, body, access_key, secret_key)
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(
                ProviderError::Other(format!("ListEndpoints status {status}: {text}")).into(),
            );
        }

        let text = resp.text().await.map_err(ProviderError::Http)?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(ProviderError::Json)?;

        // Try common Volcengine response structures
        let items = v
            .pointer("/Result/Items")
            .or_else(|| v.get("Items"))
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();

        let mut models = Vec::new();
        for item in items {
            let id = item
                .get("Id")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if id.is_empty() {
                continue;
            }
            let display = item
                .get("Name")
                .or_else(|| item.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or(id);
            models.push(ModelInfo {
                id: id.to_string(),
                display: display.to_string(),
                request_id: None,
                context_window_tokens: None,
                context_needs_pick: false,
                modalities: Vec::new(),
            });
        }

        Ok(models)
    }

    async fn signed_post(
        &self,
        client: &reqwest::Client,
        query: &str,
        body: &[u8],
        access_key: &str,
        secret_key: &str,
    ) -> Result<reqwest::Response> {
        self.signed_post_to(
            client,
            query,
            body,
            access_key,
            secret_key,
            VOLCENGINE_API_HOST,
            VOLCENGINE_SERVICE,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn signed_post_to(
        &self,
        client: &reqwest::Client,
        query: &str,
        body: &[u8],
        access_key: &str,
        secret_key: &str,
        host: &str,
        service: &str,
    ) -> Result<reqwest::Response> {
        let now = Utc::now();
        let date_str = now.format("%Y%m%d").to_string();
        let datetime_str = now.format("%Y%m%dT%H%M%SZ").to_string();
        let signed_headers = "host;x-content-sha256;x-date";
        let hashed_body = hex::encode(Sha256::digest(body));

        let canonical_request = format!(
            "POST\n/\n{query}\nhost:{host}\nx-content-sha256:{hashed_body}\nx-date:{datetime_str}\n\n{signed_headers}\n{hashed_body}"
        );
        let hashed_canonical = hex::encode(Sha256::digest(canonical_request.as_bytes()));
        let credential_scope = format!("{date_str}/{VOLCENGINE_REGION}/{service}/request");
        let string_to_sign =
            format!("HMAC-SHA256\n{datetime_str}\n{credential_scope}\n{hashed_canonical}");

        let signing_key = build_signing_key_for(secret_key, &date_str, service);
        let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

        let authorization = format!(
            "HMAC-SHA256 Credential={access_key}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"
        );

        let url = format!("https://{host}/?{query}");
        let resp = client
            .post(&url)
            .header("Host", host)
            .header("X-Date", &datetime_str)
            .header("X-Content-Sha256", &hashed_body)
            .header("Authorization", &authorization)
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(ProviderError::Http)?;

        Ok(resp)
    }

    async fn list_via_management_api(
        &self,
        client: &reqwest::Client,
        access_key: &str,
        secret_key: &str,
    ) -> Result<Vec<ModelInfo>> {
        if access_key.is_empty() || secret_key.is_empty() {
            return Err(ProviderError::Other(
                "Volcengine Access Key and Secret Key are required for model listing; add them in provider settings".to_string(),
            ).into());
        }

        // Step 1: get foundation model names
        let query = format!(
            "Action=ListFoundationModels&Version={}",
            VOLCENGINE_API_VERSION,
        );
        let body = b"{}";
        let resp = self
            .signed_post(client, &query, body, access_key, secret_key)
            .await?;
        let status = resp.status();
        let text = resp.text().await.map_err(ProviderError::Http)?;
        if !status.is_success() {
            return Err(error_from_response(status, &text).into());
        }

        let names: Vec<String> = serde_json::from_str::<serde_json::Value>(&text)
            .map_err(ProviderError::Json)?
            .pointer("/Result/Items")
            .and_then(|a| a.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("Name").and_then(|n| n.as_str()))
                    .map(|n| n.to_string())
                    .collect()
            })
            .unwrap_or_default();

        if names.is_empty() {
            return Err(ProviderError::Other(
                "Volcengine returned no foundation models; check your Access Key / Secret Key permissions".to_string(),
            ).into());
        }

        // Step 2: get versions for each foundation model
        let mut all_models = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for name in &names {
            let query = format!(
                "Action=ListFoundationModelVersions&Version={}",
                VOLCENGINE_API_VERSION,
            );
            let body = serde_json::json!({"FoundationModelName": name})
                .to_string()
                .into_bytes();
            let resp = self
                .signed_post(client, &query, &body, access_key, secret_key)
                .await?;
            let status = resp.status();
            let text = resp.text().await.map_err(ProviderError::Http)?;
            if !status.is_success() {
                continue;
            }

            let items: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&text)
                .map_err(ProviderError::Json)?
                .pointer("/Result/Items")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default();

            for item in items {
                let model_id = item
                    .get("ModelId")
                    .or_else(|| item.get("Id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| {
                        let version = item
                            .get("ModelVersion")
                            .or_else(|| item.get("Version"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("default");
                        format!("{name}-{version}")
                    });

                if seen.insert(model_id.clone()) {
                    let context_window = item
                        .pointer("/ModelInfo/MaxInputTokenLength")
                        .and_then(|v| v.as_u64());
                    all_models.push(ModelInfo {
                        id: model_id.clone(),
                        display: model_id,
                        request_id: None,
                        context_window_tokens: context_window,
                        context_needs_pick: false,
                        modalities: Vec::new(),
                    });
                }
            }
        }

        if all_models.is_empty() {
            return Err(ProviderError::Other(
                "Volcengine returned no models; check your Access Key / Secret Key permissions"
                    .to_string(),
            )
            .into());
        }

        Ok(all_models)
    }

    async fn list_via_agent_plan_api(
        &self,
        client: &reqwest::Client,
        _api_key: &str,
        access_key: &str,
        secret_key: &str,
    ) -> Result<Vec<ModelInfo>> {
        if access_key.is_empty() || secret_key.is_empty() {
            return Err(ProviderError::Other(
                "Volcengine Access Key / Secret Key required for ListArkAgentPlanModel".to_string(),
            )
            .into());
        }

        let query = format!(
            "Action=ListArkAgentPlanModel&Version={}",
            VOLCENGINE_API_VERSION,
        );
        let resp = self
            .signed_post_to(
                client,
                &query,
                b"{}",
                access_key,
                secret_key,
                VOLCENGINE_AGENT_PLAN_HOST,
                VOLCENGINE_AGENT_PLAN_SERVICE,
            )
            .await?;

        let status = resp.status();
        let text = resp.text().await.map_err(ProviderError::Http)?;
        if !status.is_success() {
            return Err(error_from_response(status, &text).into());
        }

        let items: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&text)
            .map_err(ProviderError::Json)?
            .pointer("/Result/Datas")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();

        let models: Vec<ModelInfo> = items
            .iter()
            .filter_map(|item| {
                let id = item.get("ModelID")?.as_str()?;
                if id.is_empty() {
                    return None;
                }
                Some(ModelInfo {
                    id: id.to_string(),
                    display: id.to_string(),
                    request_id: None,
                    context_window_tokens: None,
                    context_needs_pick: false,
                    modalities: Vec::new(),
                })
            })
            .collect();

        if models.is_empty() {
            return Err(ProviderError::Other(
                "ListArkAgentPlanModel returned no models".to_string(),
            )
            .into());
        }
        Ok(models)
    }
}

fn error_from_response(status: reqwest::StatusCode, text: &str) -> ProviderError {
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        ProviderError::AuthFailed(status.as_u16())
    } else {
        ProviderError::Other(format!("Volcengine API status {status}: {text}"))
    }
}

/// HMAC-SHA256 implemented manually using sha2::Sha256.
/// Avoids the `hmac` crate's `digest` version mismatch with `sha2`.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let k = if key.len() > BLOCK_SIZE {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };

    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for (i, &b) in k.iter().enumerate() {
        ipad[i] ^= b;
        opad[i] ^= b;
    }

    let inner = Sha256::digest([ipad.as_slice(), msg].concat());
    let result = Sha256::digest([opad.as_slice(), inner.as_slice()].concat());
    result.into()
}

fn build_signing_key_for(secret_key: &str, date: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(secret_key.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, VOLCENGINE_REGION.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"request").to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hmac_sha256_deterministic() {
        let a = hmac_sha256(b"key", b"msg");
        let b = hmac_sha256(b"key", b"msg");
        assert_eq!(a, b);
    }

    #[test]
    fn test_build_signing_key() {
        let key = build_signing_key_for("test-secret", "20240601", VOLCENGINE_SERVICE);
        assert!(!key.is_empty());
        assert_eq!(key.len(), 32);
    }
}
