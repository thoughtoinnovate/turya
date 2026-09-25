//! Model catalog plugin (internal, native).
//!
//! Rule 3.2: all model knowledge lives here — never in `turya-core`.
//! Sources, in priority order:
//! 1. **Live** vendor list APIs (authoritative for what is callable).
//! 2. **Cached** `models.dev` metadata (context windows, capabilities).
//! 3. **Snapshot** compiled-in fallback (first-run offline).
//! 4. **Static** per-provider curated lists (always available).
//!
//! Auth gate (hard rule): `ensure_loaded` performs ZERO network I/O for
//! providers without credentials — locked providers never phone home.
//!
//! Pricing: `models.dev` exposes no scoped pricing endpoint (only the 5 MB
//! full catalog), so `price_estimate` reports `Unknown` until one exists.
//! Callers must label prices approximate/unavailable, never invent numbers.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("io: {0}")]
    Io(String),
    #[error("bad metadata: {0}")]
    Parse(String),
}

/// Provenance of a model entry (shown as a badge in `/models`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelSource {
    Live,
    Cached,
    Snapshot,
    Static,
}

impl ModelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelSource::Live => "live",
            ModelSource::Cached => "cached",
            ModelSource::Snapshot => "snapshot",
            ModelSource::Static => "static",
        }
    }
}

/// One model with enrichment metadata (context windows verified against
/// `models.json` where stated; `None` means unknown, never guessed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedModel {
    pub id: String,
    pub display_name: String,
    pub context_window: Option<u64>,
    pub supports_tools: Option<bool>,
    pub source: ModelSource,
}

/// Subset of the `models.dev` `/models.json` schema we rely on.
#[derive(Debug, Clone, Deserialize)]
struct ModelsDevEntry {
    #[serde(default)]
    limit: Option<ModelsDevLimit>,
    #[serde(default)]
    tool_call: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelsDevLimit {
    #[serde(default)]
    context: Option<u64>,
}

/// Parsed metadata document: `lab/model-id` → facts.
#[derive(Debug, Clone, Default)]
pub struct MetadataDb {
    entries: std::collections::HashMap<String, (Option<u64>, Option<bool>)>,
}

impl MetadataDb {
    pub fn parse(json_text: &str) -> Result<Self, CatalogError> {
        let raw: std::collections::HashMap<String, ModelsDevEntry> =
            serde_json::from_str(json_text).map_err(|e| CatalogError::Parse(e.to_string()))?;
        let entries = raw
            .into_iter()
            .map(|(k, v)| (k, (v.limit.and_then(|l| l.context), v.tool_call)))
            .collect();
        Ok(Self { entries })
    }

    /// Look up facts for a bare model id, preferring `lab_hint` (e.g.
    /// `"anthropic"` for provider `anthropic`, `"google"` for `gemini`).
    pub fn lookup(&self, model_id: &str, lab_hint: Option<&str>) -> (Option<u64>, Option<bool>) {
        if let Some(lab) = lab_hint {
            let key = format!("{lab}/{model_id}");
            if let Some(found) = self.entries.get(&key) {
                return *found;
            }
        }
        let suffix = format!("/{model_id}");
        let mut candidates: Vec<&(Option<u64>, Option<bool>)> = self
            .entries
            .iter()
            .filter(|(k, _)| *k == model_id || k.ends_with(suffix.as_str()))
            .map(|(_, v)| v)
            .collect();
        candidates.sort_by_key(|(ctx, _)| std::cmp::Reverse(ctx.unwrap_or(0)));
        candidates
            .into_iter()
            .next()
            .cloned()
            .unwrap_or((None, None))
    }
}

/// Curated static fallback per provider (always available, possibly dated).
/// Context values below were verified against `models.json` on 2026-09-25;
/// anything unverified is `None` by rule (never guessed).
pub fn static_models(provider: &str) -> Vec<ResolvedModel> {
    let mk = |id: &str, ctx: Option<u64>| ResolvedModel {
        id: id.to_string(),
        display_name: id.to_string(),
        context_window: ctx,
        supports_tools: Some(true),
        source: ModelSource::Static,
    };
    match provider {
        "anthropic" => vec![
            mk("claude-sonnet-4-5", Some(200_000)),
            mk("claude-opus-4-5", Some(200_000)),
            mk("claude-haiku-4-5", None),
        ],
        "gemini" => vec![
            mk("gemini-2.5-pro", None),
            mk("gemini-2.5-flash", None),
            mk("gemini-2.0-flash", None),
        ],
        _ => vec![],
    }
}

/// Canonical models.dev lab key per provider id.
pub fn lab_hint_for(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("anthropic"),
        "gemini" => Some("google"),
        _ => None,
    }
}

const METADATA_TTL: Duration = Duration::from_secs(24 * 3600);

/// Default metadata endpoint (overridable via `TURYA_MODELS_URL`,
/// mirroring opencode's `OPENCODE_MODELS_PATH` escape hatch).
pub fn metadata_url() -> String {
    std::env::var("TURYA_MODELS_URL")
        .unwrap_or_else(|_| "https://models.dev/models.json".to_string())
}

/// Merge live ids with metadata + static fallback.
/// Live ids win; static fills gaps; metadata enriches both.
pub fn merge(
    provider: &str,
    live_ids: &[String],
    meta: Option<&MetadataDb>,
    mark_live: bool,
) -> Vec<ResolvedModel> {
    let lab = lab_hint_for(provider);
    let enrich = |id: &str, source: ModelSource| -> ResolvedModel {
        let (ctx, tools) = meta.map(|m| m.lookup(id, lab)).unwrap_or((None, None));
        ResolvedModel {
            id: id.to_string(),
            display_name: id.to_string(),
            context_window: ctx,
            supports_tools: tools,
            source,
        }
    };
    let mut out: Vec<ResolvedModel> = live_ids
        .iter()
        .map(|id| {
            enrich(
                id,
                if mark_live {
                    ModelSource::Live
                } else {
                    ModelSource::Cached
                },
            )
        })
        .collect();
    for s in static_models(provider) {
        if !out.iter().any(|m| m.id == s.id) {
            out.push(s);
        }
    }
    out
}

/// Model catalog with per-provider disk cache (`{cache_dir}/{provider}.json`).
pub struct Catalog {
    cache_dir: PathBuf,
    base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    fetched_at_secs: u64,
    models: Vec<ResolvedModel>,
}

impl Catalog {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            base_url: metadata_url(),
        }
    }

    fn cache_path(&self, provider: &str) -> PathBuf {
        self.cache_dir.join(format!("{provider}.json"))
    }

    fn read_cache(&self, provider: &str) -> Option<(Vec<ResolvedModel>, bool)> {
        let raw = std::fs::read_to_string(self.cache_path(provider)).ok()?;
        let file: CacheFile = serde_json::from_str(&raw).ok()?;
        let age = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs()
            .saturating_sub(file.fetched_at_secs);
        Some((file.models, age < METADATA_TTL.as_secs()))
    }

    fn write_cache(&self, provider: &str, models: &[ResolvedModel]) {
        let _ = std::fs::create_dir_all(&self.cache_dir);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let file = CacheFile {
            fetched_at_secs: now,
            models: models.to_vec(),
        };
        if let Ok(raw) = serde_json::to_string(&file) {
            let _ = std::fs::write(self.cache_path(provider), raw);
        }
    }

    /// Load models for a provider.
    ///
    /// * `is_authenticated` — HARD GATE: false means snapshot/static only,
    ///   zero network I/O (locked providers never phone home).
    /// * `fetch_live` — closure returning live ids (vendor API, authed).
    /// * `fetch_meta` — closure returning the metadata doc text.
    pub fn ensure_loaded(
        &self,
        provider: &str,
        is_authenticated: bool,
        fetch_live: impl FnOnce() -> Vec<String>,
        fetch_meta: impl FnOnce(&str) -> Result<String, String>,
    ) -> Vec<ResolvedModel> {
        if !is_authenticated {
            return static_models(provider)
                .into_iter()
                .map(|mut m| {
                    m.source = ModelSource::Snapshot;
                    m
                })
                .collect();
        }
        let live = fetch_live();
        let meta = fetch_meta(&self.base_url)
            .ok()
            .and_then(|text| MetadataDb::parse(&text).ok());
        if meta.is_some() || !live.is_empty() {
            let merged = merge(provider, &live, meta.as_ref(), true);
            self.write_cache(provider, &merged);
            return merged;
        }
        // Offline: fresh cache, else snapshot.
        if let Some((cached, fresh)) = self.read_cache(provider) {
            if fresh || !cached.is_empty() {
                return cached
                    .into_iter()
                    .map(|mut m| {
                        m.source = ModelSource::Cached;
                        m
                    })
                    .collect();
            }
        }
        static_models(provider)
    }

    /// Host-function surface for other plugins: JSON model list.
    pub fn query_models_json(&self, provider: &str, is_authenticated: bool) -> String {
        let models = self.ensure_loaded(provider, is_authenticated, Vec::new, |_| {
            Err("offline".to_string())
        });
        serde_json::to_string(&models).unwrap_or_else(|_| "[]".to_string())
    }

    /// Price info: unavailable until a scoped pricing source exists.
    /// Returns `None` rather than inventing numbers (callers label accordingly).
    pub fn price_estimate(&self, _model_id: &str, _input_tokens: u64) -> Option<PriceEstimate> {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceEstimate {
    pub input_cost_usd: f64,
    pub output_cost_usd: f64,
    pub approximate: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    const MINI_META: &str = r#"{
        "anthropic/claude-sonnet-4-5": {"limit": {"context": 200000, "output": 64000}, "tool_call": true},
        "google/gemini-2.5-pro": {"limit": {"context": 1048576}, "tool_call": true}
    }"#;

    #[test]
    fn metadata_lookup_prefers_lab_hint() {
        let db = MetadataDb::parse(MINI_META).unwrap();
        assert_eq!(
            db.lookup("gemini-2.5-pro", Some("google")),
            (Some(1_048_576), Some(true))
        );
        // Unknown id yields unknowns, never an error.
        assert_eq!(db.lookup("nope-9", Some("google")), (None, None));
        // Malformed doc is an error (caller falls back).
        assert!(MetadataDb::parse("{oops").is_err());
    }

    #[test]
    fn merge_live_wins_static_fills_gaps() {
        let db = MetadataDb::parse(MINI_META).unwrap();
        let merged = merge(
            "anthropic",
            &["claude-sonnet-4-5".to_string()],
            Some(&db),
            true,
        );
        let live: Vec<_> = merged
            .iter()
            .filter(|m| m.source == ModelSource::Live)
            .collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].context_window, Some(200_000));
        // Static-only siblings still listed.
        assert!(merged.iter().any(|m| m.source == ModelSource::Static));
    }

    #[test]
    fn auth_gate_blocks_all_network_for_locked_providers() {
        let dir = std::env::temp_dir().join("turya-catalog-gate-test");
        let _ = std::fs::remove_dir_all(&dir);
        let cat = Catalog::new(dir);
        let live_calls = Rc::new(Cell::new(0));
        let meta_calls = Rc::new(Cell::new(0));
        let models = cat.ensure_loaded(
            "gemini",
            false, // locked
            || {
                live_calls.set(live_calls.get() + 1);
                vec!["x".to_string()]
            },
            |_| {
                meta_calls.set(meta_calls.get() + 1);
                Ok(MINI_META.to_string())
            },
        );
        assert_eq!(live_calls.get(), 0);
        assert_eq!(meta_calls.get(), 0);
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| m.source == ModelSource::Snapshot));
    }

    #[test]
    fn offline_uses_cache_then_snapshot() {
        let dir = std::env::temp_dir().join("turya-catalog-cache-test");
        let _ = std::fs::remove_dir_all(&dir);
        let cat = Catalog::new(dir);
        // Seed via a successful load with injected fetchers.
        let seeded = cat.ensure_loaded(
            "anthropic",
            true,
            || vec!["claude-sonnet-4-5".to_string()],
            |_| Ok(MINI_META.to_string()),
        );
        assert!(seeded.iter().any(|m| m.source == ModelSource::Live));
        // Now offline: fetchers fail -> fresh cache served as Cached.
        let offline = cat.ensure_loaded("anthropic", true, Vec::new, |_| Err("down".to_string()));
        assert!(offline.iter().all(|m| m.source == ModelSource::Cached));
        assert!(offline.iter().any(|m| m.id == "claude-sonnet-4-5"));
    }

    #[test]
    fn price_is_honestly_unavailable() {
        let dir = std::env::temp_dir().join("turya-catalog-price-test");
        let cat = Catalog::new(dir);
        assert!(cat.price_estimate("gemini-2.5-pro", 1000).is_none());
        let json = cat.query_models_json("anthropic", false);
        let models: Vec<ResolvedModel> = serde_json::from_str(&json).unwrap();
        assert!(!models.is_empty());
    }
}
