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
//! providers without credentials — locked providers never phone home. The
//! gate is a caller-supplied `CatalogNetwork`, never inferred here: the host
//! owns credential state, so the host decides what this plugin may reach for.
//! A provider that needs no credential (a server the user already runs) is
//! the one exception, and it gets live discovery only — the `models.dev`
//! fetch is a third-party call and stays gated on the user having some
//! credential configured.
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

/// How a model exposes reasoning, if at all.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReasoningInfo {
    /// The model can reason at all. `false` and "unknown" are different:
    /// a false badge is a claim, so it is only set from real data.
    pub supported: Option<bool>,
    /// Request-time controls: `effort` levels and/or a token budget.
    pub efforts: Vec<String>,
    /// Minimum reasoning token budget, when the source states one.
    pub min_budget_tokens: Option<u64>,
}

impl ReasoningInfo {
    pub fn supports_effort(&self, level: &str) -> bool {
        self.efforts.iter().any(|e| e.eq_ignore_ascii_case(level))
    }
}

/// One model with enrichment metadata (context windows verified against
/// `models.json` where stated; `None` means unknown, never guessed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedModel {
    pub id: String,
    pub display_name: String,
    pub context_window: Option<u64>,
    /// Usable input tokens (context minus reserved output), when known.
    pub input_limit: Option<u64>,
    /// Maximum output tokens, when known.
    pub output_limit: Option<u64>,
    pub supports_tools: Option<bool>,
    /// Accepts file/image attachments.
    pub supports_attachments: Option<bool>,
    pub reasoning: ReasoningInfo,
    /// Input modalities the model accepts (e.g. text, image, pdf).
    pub input_modalities: Vec<String>,
    pub source: ModelSource,
}

/// Subset of the `models.dev` `/models.json` schema we rely on.
///
/// Two shapes of the reasoning metadata are accepted: the current
/// `reasoning` + `reasoning_options` pair, and the v2 `reasoning` object
/// with `supported`/`options`. Both are read forward (Rule 5.2 is about
/// shims for *old callers*, not about tolerating a source that publishes
/// two shapes today).
#[derive(Debug, Clone, Deserialize)]
struct ModelsDevEntry {
    #[serde(default)]
    limit: Option<ModelsDevLimit>,
    #[serde(default)]
    tool_call: Option<bool>,
    #[serde(default)]
    attachment: Option<bool>,
    #[serde(default)]
    reasoning: Option<serde_json::Value>,
    #[serde(default)]
    reasoning_options: Option<Vec<ModelsDevReasoningOption>>,
    #[serde(default)]
    modalities: Option<ModelsDevModalities>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelsDevLimit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    input: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelsDevModalities {
    #[serde(default)]
    input: Option<Vec<String>>,
}

/// One `reasoning_options` entry: `{"type":"effort","values":[...]}` or
/// `{"type":"budget_tokens","min":1024}`.
#[derive(Debug, Clone, Deserialize)]
struct ModelsDevReasoningOption {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    values: Option<Vec<String>>,
    #[serde(default)]
    min: Option<u64>,
}

/// The facts we keep per `lab/model-id`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelFacts {
    pub context: Option<u64>,
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub tool_call: Option<bool>,
    pub attachment: Option<bool>,
    pub reasoning: ReasoningInfo,
    pub input_modalities: Vec<String>,
}

impl ModelFacts {
    fn from_entry(entry: &ModelsDevEntry) -> Self {
        let mut reasoning = ReasoningInfo::default();
        if let Some(raw) = &entry.reasoning {
            // v2 shape: {"supported": bool, "options": [...]}
            if let Some(supported) = raw.get("supported").and_then(|v| v.as_bool()) {
                reasoning.supported = Some(supported);
            }
            if let Some(options) = raw.get("options").and_then(|v| v.as_array()) {
                for opt in options {
                    apply_reasoning_option(&mut reasoning, opt);
                }
            }
            // current shape: reasoning is a bare bool
            if let Some(b) = raw.as_bool() {
                reasoning.supported = Some(b);
            }
        }
        for opt in entry.reasoning_options.iter().flatten() {
            match opt.kind.as_str() {
                "effort" => {
                    for v in opt.values.iter().flatten() {
                        if !reasoning.efforts.iter().any(|e| e == v) {
                            reasoning.efforts.push(v.clone());
                        }
                    }
                }
                "budget_tokens" => {
                    if let Some(min) = opt.min {
                        reasoning.min_budget_tokens = Some(min);
                    }
                }
                _ => {}
            }
        }
        if reasoning.supported.is_none()
            && (!reasoning.efforts.is_empty() || reasoning.min_budget_tokens.is_some())
        {
            // Controls exist, so the model reasons.
            reasoning.supported = Some(true);
        }
        Self {
            context: entry.limit.as_ref().and_then(|l| l.context),
            input: entry.limit.as_ref().and_then(|l| l.input),
            output: entry.limit.as_ref().and_then(|l| l.output),
            tool_call: entry.tool_call,
            attachment: entry.attachment,
            reasoning,
            input_modalities: entry
                .modalities
                .as_ref()
                .and_then(|m| m.input.clone())
                .unwrap_or_default(),
        }
    }
}

fn apply_reasoning_option(reasoning: &mut ReasoningInfo, opt: &serde_json::Value) {
    match opt.get("type").and_then(|v| v.as_str()) {
        Some("effort") => {
            if let Some(values) = opt.get("values").and_then(|v| v.as_array()) {
                for v in values.iter().filter_map(|v| v.as_str()) {
                    if !reasoning.efforts.iter().any(|e| e == v) {
                        reasoning.efforts.push(v.to_string());
                    }
                }
            }
        }
        Some("budget_tokens") => {
            if let Some(min) = opt.get("min").and_then(|v| v.as_u64()) {
                reasoning.min_budget_tokens = Some(min);
            }
        }
        _ => {}
    }
}

/// Parsed metadata document: `lab/model-id` → facts.
#[derive(Debug, Clone, Default)]
pub struct MetadataDb {
    entries: std::collections::HashMap<String, ModelFacts>,
}

impl MetadataDb {
    pub fn parse(json_text: &str) -> Result<Self, CatalogError> {
        let raw: std::collections::HashMap<String, ModelsDevEntry> =
            serde_json::from_str(json_text).map_err(|e| CatalogError::Parse(e.to_string()))?;
        let entries = raw
            .iter()
            .map(|(k, v)| (k.clone(), ModelFacts::from_entry(v)))
            .collect();
        Ok(Self { entries })
    }

    /// Look up facts for a bare model id, preferring `lab_hint` (e.g.
    /// `"anthropic"` for provider `anthropic`, `"google"` for `gemini`).
    /// Full fact record for a model, preferring `lab_hint` (e.g.
    /// `"anthropic"` for provider `anthropic`, `"google"` for `gemini`).
    pub fn facts(&self, model_id: &str, lab_hint: Option<&str>) -> Option<ModelFacts> {
        if let Some(lab) = lab_hint {
            let key = format!("{lab}/{model_id}");
            if let Some(found) = self.entries.get(&key) {
                return Some(found.clone());
            }
        }
        let suffix = format!("/{model_id}");
        let mut candidates: Vec<&ModelFacts> = self
            .entries
            .iter()
            .filter(|(k, _)| *k == model_id || k.ends_with(suffix.as_str()))
            .map(|(_, v)| v)
            .collect();
        // Prefer the richest match: a known context window beats a stub.
        candidates.sort_by_key(|f| std::cmp::Reverse(f.context.unwrap_or(0)));
        candidates.into_iter().next().cloned()
    }

    /// Context window + tool support (the two facts the old API exposed).
    pub fn lookup(&self, model_id: &str, lab_hint: Option<&str>) -> (Option<u64>, Option<bool>) {
        match self.facts(model_id, lab_hint) {
            Some(f) => (f.context, f.tool_call),
            None => (None, None),
        }
    }
}

/// Curated static fallback per provider (always available, possibly dated).
/// Context values below were verified against `models.json` on 2026-09-25;
/// anything unverified is `None` by rule (never guessed).
pub fn static_models(provider: &str) -> Vec<ResolvedModel> {
    let mk = |id: &str, ctx: Option<u64>| ResolvedModel {
        input_limit: None,
        output_limit: None,
        supports_attachments: None,
        reasoning: ReasoningInfo::default(),
        input_modalities: Vec::new(),
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
        // Unknown ids fall through to an empty list, and that is CORRECT for
        // a local provider (`ollama`): the only models that exist are the ones
        // on the user's own disk, discoverable through
        // `ProviderPlugin::list_models`. Never add a hardcoded list of local
        // model names here — a stale list shadows live detection, so the
        // browser would offer models the server does not have and hide the
        // ones it does.
        _ => vec![],
    }
}

/// Canonical models.dev lab key per provider id.
pub fn lab_hint_for(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("anthropic"),
        "gemini" => Some("google"),
        // `None` for a local provider (`ollama`) is right, not an oversight:
        // models.dev publishes nothing for a server the user runs, so there
        // is no lab to hint at. Do not "fix" this by inventing one.
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
        let facts = meta.and_then(|m| m.facts(id, lab)).unwrap_or_default();
        ResolvedModel {
            id: id.to_string(),
            display_name: id.to_string(),
            context_window: facts.context,
            input_limit: facts.input,
            output_limit: facts.output,
            supports_tools: facts.tool_call,
            supports_attachments: facts.attachment,
            reasoning: facts.reasoning,
            input_modalities: facts.input_modalities,
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

/// What `ensure_loaded` is allowed to reach for on one provider's behalf.
///
/// The catalog never resolves credentials and must not infer auth policy
/// (Rule 3.1: no auth logic in the kernel, and none invented in a plugin).
/// The caller holds the credential state and passes the decision in, so the
/// two reachability questions stay separate:
///
/// * `live` — the provider's own `list_models` call. Allowed for a
///   credentialed provider and for a provider that needs no credential at
///   all (a server the user already runs), blocked for a locked one.
/// * `metadata` — the third-party `models.dev` document fetch. Keyed to the
///   user having *some* credential configured: a user who has authenticated
///   nothing does not start phoning a third party, so a keyless setup stays
///   silent even though its local provider is discoverable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogNetwork {
    pub live: bool,
    pub metadata: bool,
}

impl CatalogNetwork {
    /// Nothing leaves the machine: a provider that needs a credential the
    /// user has not configured.
    pub const fn none() -> Self {
        Self {
            live: false,
            metadata: false,
        }
    }

    /// Live discovery only: a provider that needs no credential.
    pub const fn live_only() -> Self {
        Self {
            live: true,
            metadata: false,
        }
    }

    /// Both: a provider with a credential in hand.
    pub const fn full() -> Self {
        Self {
            live: true,
            metadata: true,
        }
    }
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

    /// Offline model ids from disk cache (any age — stale beats rejection).
    /// Pure disk read, never network. Used to validate switches offline.
    pub fn cached_ids(&self, provider: &str) -> Vec<String> {
        self.read_cache(provider)
            .map(|(models, _)| models.into_iter().map(|m| m.id).collect())
            .unwrap_or_default()
    }

    /// Models for a provider from the disk cache, with their facts. Offline
    /// and never network: used to answer "how big is this model's window?"
    /// without a round trip. Empty when nothing is cached.
    pub fn cached_models(&self, provider: &str) -> Vec<ResolvedModel> {
        self.read_cache(provider)
            .map(|(models, _)| models)
            .unwrap_or_default()
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
    /// * `net` — what this load may reach for (see `CatalogNetwork`). The
    ///   caller decides: `live: false` means zero fetcher calls at all (a
    ///   locked provider never phones home), while `live: true,
    ///   metadata: false` still runs live discovery — the only way a
    ///   no-credential local provider's models are ever known.
    /// * `fetch_live` — closure returning live ids (the provider's own API).
    /// * `fetch_meta` — closure returning the metadata doc text.
    pub fn ensure_loaded(
        &self,
        provider: &str,
        net: CatalogNetwork,
        fetch_live: impl FnOnce() -> Vec<String>,
        fetch_meta: impl FnOnce(&str) -> Result<String, String>,
    ) -> Vec<ResolvedModel> {
        if !net.live {
            return static_models(provider)
                .into_iter()
                .map(|mut m| {
                    m.source = ModelSource::Snapshot;
                    m
                })
                .collect();
        }
        let live = fetch_live();
        let meta = if net.metadata {
            fetch_meta(&self.base_url)
                .ok()
                .and_then(|text| MetadataDb::parse(&text).ok())
        } else {
            None
        };
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
    ///
    /// Facts only — this surface has no live fetcher, so `net.live` is not
    /// used here; only `net.metadata` decides whether the document is read.
    /// Callers that own a provider plugin drive discovery themselves.
    pub fn query_models_json(&self, provider: &str, net: CatalogNetwork) -> String {
        let models = self.ensure_loaded(provider, net, Vec::new, |_| Err("offline".to_string()));
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
            CatalogNetwork::none(), // locked
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
    fn no_credential_provider_gets_live_discovery_but_no_metadata() {
        // A local server (ollama) needs no credential. Its models exist only
        // on the user's disk, so live discovery is the ONLY way they are ever
        // known — gating it on "authenticated" showed an empty list forever.
        // The models.dev fetch is a third party, so it stays blocked.
        let dir = std::env::temp_dir().join("turya-catalog-noauth-live-test");
        let _ = std::fs::remove_dir_all(&dir);
        let cat = Catalog::new(dir);
        let live_calls = Rc::new(Cell::new(0));
        let meta_calls = Rc::new(Cell::new(0));
        let models = cat.ensure_loaded(
            "ollama",
            CatalogNetwork::live_only(),
            || {
                live_calls.set(live_calls.get() + 1);
                vec![
                    "llama3.2:latest".to_string(),
                    "qwen2.5-coder:7b".to_string(),
                ]
            },
            |_| {
                meta_calls.set(meta_calls.get() + 1);
                Ok(MINI_META.to_string())
            },
        );
        assert_eq!(live_calls.get(), 1, "live discovery must still run");
        assert_eq!(meta_calls.get(), 0, "models.dev must not be phoned home to");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["llama3.2:latest", "qwen2.5-coder:7b"]);
        assert!(models.iter().all(|m| m.source == ModelSource::Live));
    }

    #[test]
    fn local_provider_has_no_static_models_to_shadow_discovery() {
        // Guard for the comment in `static_models`: the only source of truth
        // for a local provider is the server itself.
        assert!(static_models("ollama").is_empty());
        assert_eq!(lab_hint_for("ollama"), None);
    }

    #[test]
    fn offline_uses_cache_then_snapshot() {
        let dir = std::env::temp_dir().join("turya-catalog-cache-test");
        let _ = std::fs::remove_dir_all(&dir);
        let cat = Catalog::new(dir);
        // Seed via a successful load with injected fetchers.
        let seeded = cat.ensure_loaded(
            "anthropic",
            CatalogNetwork::full(),
            || vec!["claude-sonnet-4-5".to_string()],
            |_| Ok(MINI_META.to_string()),
        );
        assert!(seeded.iter().any(|m| m.source == ModelSource::Live));
        // Now offline: fetchers fail -> fresh cache served as Cached.
        let offline = cat.ensure_loaded("anthropic", CatalogNetwork::full(), Vec::new, |_| {
            Err("down".to_string())
        });
        assert!(offline.iter().all(|m| m.source == ModelSource::Cached));
        assert!(offline.iter().any(|m| m.id == "claude-sonnet-4-5"));
    }

    #[test]
    fn price_is_honestly_unavailable() {
        let dir = std::env::temp_dir().join("turya-catalog-price-test");
        let cat = Catalog::new(dir);
        assert!(cat.price_estimate("gemini-2.5-pro", 1000).is_none());
        let json = cat.query_models_json("anthropic", CatalogNetwork::none());
        let models: Vec<ResolvedModel> = serde_json::from_str(&json).unwrap();
        assert!(!models.is_empty());
    }

    #[test]
    fn cached_ids_reads_disk_without_network() {
        let dir = std::env::temp_dir().join("turya-catalog-cached-ids-test");
        let _ = std::fs::remove_dir_all(&dir);
        let cat = Catalog::new(dir);
        // Nothing cached yet.
        assert!(cat.cached_ids("gemini").is_empty());
        // Seed via injected fetchers (no network), then read back offline.
        let _ = cat.ensure_loaded(
            "gemini",
            CatalogNetwork::full(),
            || vec!["gemini-flash-latest".to_string()],
            |_| Ok(MINI_META.to_string()),
        );
        assert!(cat
            .cached_ids("gemini")
            .contains(&"gemini-flash-latest".to_string()));
    }
}

#[cfg(test)]
mod enrichment_tests {
    use super::*;

    const RICH: &str = r#"{
        "anthropic/claude-opus-4-6": {
            "limit": { "context": 200000, "input": 160000, "output": 64000 },
            "tool_call": true,
            "attachment": true,
            "reasoning": true,
            "reasoning_options": [
                { "type": "effort", "values": ["low", "medium", "high", "max"] },
                { "type": "budget_tokens", "min": 1024 }
            ],
            "modalities": { "input": ["text", "image", "pdf"] }
        },
        "google/gemini-2.5-pro": {
            "limit": { "context": 1048576 },
            "tool_call": true,
            "reasoning": { "supported": true, "options": [ { "type": "effort", "values": ["low", "high"] } ] }
        },
        "google/gemini-flash-lite-latest": {
            "limit": { "context": 1048576 },
            "tool_call": true
        }
    }"#;

    #[test]
    fn reads_context_input_and_output_limits() {
        let db = MetadataDb::parse(RICH).unwrap();
        let f = db.facts("claude-opus-4-6", Some("anthropic")).unwrap();
        assert_eq!(f.context, Some(200_000));
        assert_eq!(f.input, Some(160_000));
        assert_eq!(f.output, Some(64_000));
    }

    #[test]
    fn reads_attachment_and_modalities() {
        let db = MetadataDb::parse(RICH).unwrap();
        let f = db.facts("claude-opus-4-6", Some("anthropic")).unwrap();
        assert_eq!(f.attachment, Some(true));
        assert_eq!(f.input_modalities, vec!["text", "image", "pdf"]);
    }

    #[test]
    fn reads_effort_levels_and_budget_floor() {
        let db = MetadataDb::parse(RICH).unwrap();
        let f = db.facts("claude-opus-4-6", Some("anthropic")).unwrap();
        assert_eq!(f.reasoning.supported, Some(true));
        assert_eq!(f.reasoning.efforts, vec!["low", "medium", "high", "max"]);
        assert_eq!(f.reasoning.min_budget_tokens, Some(1024));
        assert!(f.reasoning.supports_effort("high"));
        assert!(!f.reasoning.supports_effort("extreme"));
    }

    #[test]
    fn reads_the_v2_reasoning_object_shape() {
        // models.dev v2 nests supported/options under `reasoning`.
        let db = MetadataDb::parse(RICH).unwrap();
        let f = db.facts("gemini-2.5-pro", Some("google")).unwrap();
        assert_eq!(f.reasoning.supported, Some(true));
        assert_eq!(f.reasoning.efforts, vec!["low", "high"]);
    }

    #[test]
    fn a_model_without_reasoning_metadata_stays_unknown() {
        // Honest absence: no badge, not a false "does not support reasoning".
        let db = MetadataDb::parse(RICH).unwrap();
        let f = db
            .facts("gemini-flash-lite-latest", Some("google"))
            .unwrap();
        assert_eq!(f.reasoning.supported, None);
        assert!(f.reasoning.efforts.is_empty());
        assert_eq!(f.input_modalities.len(), 0);
    }

    #[test]
    fn effort_controls_imply_reasoning_support() {
        let json = r#"{"x/y-z": {"reasoning_options": [{"type":"effort","values":["high"]}]}}"#;
        let db = MetadataDb::parse(json).unwrap();
        let f = db.facts("y-z", Some("x")).unwrap();
        assert_eq!(f.reasoning.supported, Some(true));
    }

    #[test]
    fn merge_carries_the_new_facts_through() {
        let db = MetadataDb::parse(RICH).unwrap();
        let models = merge(
            "anthropic",
            &["claude-opus-4-6".to_string()],
            Some(&db),
            true,
        );
        let m = &models[0];
        assert_eq!(m.context_window, Some(200_000));
        assert_eq!(m.input_limit, Some(160_000));
        assert_eq!(m.supports_attachments, Some(true));
        assert!(m.reasoning.supports_effort("max"));
        assert_eq!(m.input_modalities, vec!["text", "image", "pdf"]);
    }

    #[test]
    fn an_unknown_model_reports_no_facts_rather_than_guessing() {
        let db = MetadataDb::parse(RICH).unwrap();
        assert!(db.facts("mystery-model", Some("anthropic")).is_none());
        let models = merge("anthropic", &["mystery-model".to_string()], Some(&db), true);
        assert_eq!(models[0].context_window, None);
        assert_eq!(models[0].reasoning.supported, None);
    }
}
