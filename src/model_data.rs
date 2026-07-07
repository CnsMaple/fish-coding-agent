use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

const MODELS_DEV_URL: &str = "https://models.dev/models.json";
const CACHE_TTL_HOURS: i64 = 24;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelData {
    pub models: HashMap<String, ModelEntry>,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output: Option<u64>,
    #[serde(default)]
    pub modalities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextOption {
    pub context: u64,
    pub modalities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevModel {
    #[serde(default)]
    limit: Option<ModelsDevLimit>,
    #[serde(default)]
    modalities: Option<ModelsDevModalities>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevModalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct ModelsDevLimit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

impl ModelData {
    pub fn load(path: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(raw) = serde_json::to_string(self) {
            let _ = std::fs::write(path, &raw);
        }
    }

    pub fn is_stale(&self) -> bool {
        let age = chrono::Utc::now() - self.fetched_at;
        age.num_hours() >= CACHE_TTL_HOURS
    }

    /// Look up the context window for a model by matching the API model ID
    /// against all models.dev entries. Tries exact match first, then
    /// prefix match (model_id starts with the models.dev model name).
    pub fn lookup(&self, _provider_prefix: &str, model_id: &str) -> Option<u64> {
        let model_id_lower = model_id.to_lowercase();

        let mut best_match: Option<(usize, u64)> = None;
        for (key, entry) in &self.models {
            let Some(ctx) = entry.context_window else {
                continue;
            };
            let key_lower = key.to_lowercase();
            let Some(model_name) = key_lower.split_once('/').map(|(_, name)| name) else {
                continue;
            };
            if model_name.is_empty() {
                continue;
            }
            // Exact match
            if model_id_lower == model_name {
                return Some(ctx);
            }
            // Prefix match: model_id starts with the models.dev model name
            if model_id_lower.starts_with(model_name) {
                let match_len = model_name.len();
                if best_match.is_none_or(|(len, _)| match_len > len) {
                    best_match = Some((match_len, ctx));
                }
            }
        }
        best_match.map(|(_, ctx)| ctx)
    }

    /// Get unique context window + modality combinations for all models of
    /// a given provider. Deduplicates by both context and modalities.
    /// Returns a sorted list (ascending by context) for display in the picker.
    pub fn context_options_for_provider(&self, provider_prefix: &str) -> Vec<ContextOption> {
        let prefix_lower = format!("{}/", provider_prefix.to_lowercase());
        let mut seen = std::collections::HashSet::new();
        let mut options: Vec<ContextOption> = Vec::new();
        for (key, entry) in &self.models {
            if !key.to_lowercase().starts_with(&prefix_lower) {
                continue;
            }
            let Some(ctx) = entry.context_window else {
                continue;
            };
            let mut mods = entry.modalities.clone();
            mods.sort();
            let key = (ctx, mods.join(","));
            if seen.insert(key) {
                options.push(ContextOption {
                    context: ctx,
                    modalities: entry.modalities.clone(),
                });
            }
        }
        options.sort_by_key(|o| o.context);
        options
    }
}

pub async fn fetch_models_dev(
    _client: &reqwest::Client,
    cache_path: &Path,
) -> Result<ModelData, String> {
    // Try cache first
    if let Some(cached) = ModelData::load(cache_path) {
        if !cached.is_stale() {
            return Ok(cached);
        }
    }

    // Fetch from models.dev — use a fresh client with a longer timeout
    // because the 3MB+ JSON payload can take a while on slow connections.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;

    let resp = client
        .get(MODELS_DEV_URL)
        .send()
        .await
        .map_err(|e| format!("models.dev download failed: {e}"))?;

    let raw: HashMap<String, ModelsDevModel> = resp
        .json()
        .await
        .map_err(|e| format!("parse failed: {e}"))?;

    let mut models = HashMap::new();
    for (id, m) in raw {
        let entry = ModelEntry {
            context_window: m.limit.as_ref().and_then(|l| l.context),
            max_output: m.limit.as_ref().and_then(|l| l.output),
            modalities: m
                .modalities
                .map(|mods| mods.input)
                .unwrap_or_default(),
        };
        models.insert(id, entry);
    }

    let data = ModelData {
        models,
        fetched_at: chrono::Utc::now(),
    };
    data.save(cache_path);
    Ok(data)
}

/// A persistent cache of user-chosen context windows for custom models.
/// Keyed by model ID, stored as JSON alongside the model-data cache.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CustomContextCache {
    pub entries: HashMap<String, u64>,
}

impl CustomContextCache {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(raw) = serde_json::to_string(self) {
            let _ = std::fs::write(path, &raw);
        }
    }

    pub fn get(&self, model_id: &str) -> Option<u64> {
        self.entries.get(model_id).copied()
    }

    pub fn set(&mut self, model_id: String, context: u64, path: &Path) {
        self.entries.insert(model_id, context);
        self.save(path);
    }
}