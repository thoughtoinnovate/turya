//! Durable settings at `$TURYA_HOME/config.toml` (internal host plugin).
//!
//! Precedence: CLI flag > environment > this file > built-in default. The file
//! carries a `version` tag; a foreign or unparsable file is reported and
//! replaced, never migrated (AGENTS.md Rule 5.3/5.4). Unknown keys are ignored
//! so a file written by a newer build cannot break this one.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Current settings schema. Bump freely; old files are rejected, not migrated.
pub const CONFIG_VERSION: u32 = 1;

/// One MCP server the user configured. A command and its arguments: all the
/// stdio transport needs, since that is a host path plus how to run it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuryaConfig {
    /// Schema tag. Read only to produce a good error message.
    pub version: u32,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// `open` | `review-for-me` | `manual`.
    pub permission_mode: Option<String>,
    /// Per-turn step budgets (`/steps`).
    pub max_steps: Option<usize>,
    pub max_tool_calls: Option<u32>,
    /// Reasoning effort for subsequent turns (`/efforts`).
    pub effort: Option<String>,
    /// Automatic context compaction (Phase B).
    pub auto_compact: Option<bool>,
    /// Approximate recent tokens kept verbatim beside a compaction summary.
    pub keep_tokens: Option<u32>,
    /// What a prompt sent during a running turn does.
    pub queue_behavior: Option<String>,
    pub show_thinking: Option<bool>,
    /// `turya update` / `upgrade` mouse wheel support. `auto` probes the
    /// terminal; `on` forces it; `off` keeps native text selection.
    pub mouse: Option<String>,
    /// MCP servers to connect at startup.
    pub mcp_servers: Option<Vec<McpServerConfig>>,
    /// Base URL of a local Ollama daemon. The plugin reads `OLLAMA_HOST`;
    /// this bridges the saved value into it, and an explicit environment
    /// variable takes precedence.
    pub ollama_host: Option<String>,
    /// Extra skill directories.
    pub skills_paths: Option<Vec<String>>,
    /// The theme, stored as the UI plugin handed it over. The host does not
    /// read it: it is an opaque table in the settings file, and only the UI
    /// knows what a theme means.
    pub theme: Option<toml::Value>,
    /// Set when the user asks for no colour at all (`NO_COLOR`).
    pub no_color: Option<bool>,
    /// Follow new output to the bottom of the transcript.
    pub auto_scroll: Option<bool>,
}

impl Default for TuryaConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            provider: None,
            model: None,
            permission_mode: None,
            max_steps: None,
            max_tool_calls: None,
            effort: None,
            auto_compact: Some(true),
            keep_tokens: Some(15_000),
            queue_behavior: Some("steer".to_string()),
            show_thinking: Some(true),
            mouse: Some("auto".to_string()),
            ollama_host: None,
            mcp_servers: None,
            skills_paths: None,
            theme: None,
            no_color: Some(false),
            auto_scroll: Some(true),
        }
    }
}

/// What happened while loading, so the caller can tell the user rather than
/// silently discarding their settings.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadOutcome {
    /// No file yet: defaults are in force.
    Missing,
    Loaded,
    /// The file was from another schema version and has been replaced.
    Replaced {
        found: u32,
        backup: String,
    },
    /// The file was unreadable or malformed and has been replaced.
    ReplacedUnparsable {
        reason: String,
        backup: String,
    },
}

impl TuryaConfig {
    /// Load settings, replacing an incompatible file (Rule 5.3).
    pub fn load(path: impl AsRef<Path>) -> (TuryaConfig, Option<LoadOutcome>) {
        let path = path.as_ref();
        let raw = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => return (TuryaConfig::default(), None),
        };
        // Probe just the version tag first, so an incompatible file is
        // reported as "wrong version" rather than as a parse failure.
        #[derive(Deserialize)]
        struct VersionProbe {
            version: Option<u32>,
        }
        let probe: VersionProbe = match toml::from_str(&raw) {
            Ok(p) => p,
            Err(e) => {
                let backup = backup_path(path);
                let _ = std::fs::rename(path, &backup);
                return (
                    TuryaConfig::default(),
                    Some(LoadOutcome::ReplacedUnparsable {
                        reason: e.to_string(),
                        backup: backup.to_string_lossy().to_string(),
                    }),
                );
            }
        };
        let found = probe.version.unwrap_or(0);
        if found != CONFIG_VERSION {
            let backup = backup_path(path);
            let _ = std::fs::rename(path, &backup);
            return (
                TuryaConfig::default(),
                Some(LoadOutcome::Replaced {
                    found,
                    backup: backup.to_string_lossy().to_string(),
                }),
            );
        }
        match toml::from_str::<TuryaConfig>(&raw) {
            Ok(cfg) => (cfg, Some(LoadOutcome::Loaded)),
            Err(e) => {
                let backup = backup_path(path);
                let _ = std::fs::rename(path, &backup);
                (
                    TuryaConfig::default(),
                    Some(LoadOutcome::ReplacedUnparsable {
                        reason: e.to_string(),
                        backup: backup.to_string_lossy().to_string(),
                    }),
                )
            }
        }
    }

    /// Write settings atomically: a crash mid-write must not leave a
    /// half-written file that the next launch has to discard.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), String> {
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create config dir: {e}"))?;
        }
        let body = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body).map_err(|e| format!("cannot write config: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace config: {e}"))?;
        Ok(())
    }

    /// Effective colour mode. `NO_COLOR` in the environment wins over the
    /// file, because that is the convention every other CLI honours.
    pub fn color_enabled(&self) -> bool {
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        !self.no_color.unwrap_or(false)
    }
}

/// Turn an opaque theme snapshot into TOML so it can live in the settings
/// file next to everything else.
///
/// The host never looks inside a theme, so this conversion is deliberately
/// dumb: JSON objects become tables, arrays become arrays, and anything TOML
/// cannot say (a null, a date) is refused rather than mangled.
pub fn theme_json_to_toml(value: &serde_json::Value) -> Result<toml::Value, String> {
    match value {
        serde_json::Value::Object(map) => {
            let mut table = toml::map::Map::new();
            for (key, item) in map {
                table.insert(key.clone(), theme_json_to_toml(item)?);
            }
            Ok(toml::Value::Table(table))
        }
        serde_json::Value::Array(items) => {
            let mut array = Vec::with_capacity(items.len());
            for item in items {
                array.push(theme_json_to_toml(item)?);
            }
            Ok(toml::Value::Array(array))
        }
        serde_json::Value::String(text) => Ok(toml::Value::String(text.clone())),
        serde_json::Value::Bool(flag) => Ok(toml::Value::Boolean(*flag)),
        serde_json::Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                Ok(toml::Value::Integer(int))
            } else if let Some(float) = number.as_f64() {
                Ok(toml::Value::Float(float))
            } else {
                Err(format!(
                    "cannot store the number {number} in the settings file"
                ))
            }
        }
        serde_json::Value::Null => Err("the theme must not contain empty values".to_string()),
    }
}

/// Read a stored theme back out of the settings file.
pub fn theme_toml_to_json(value: &toml::Value) -> Result<serde_json::Value, String> {
    match value {
        toml::Value::Table(table) => {
            let mut map = serde_json::Map::new();
            for (key, item) in table {
                map.insert(key.clone(), theme_toml_to_json(item)?);
            }
            Ok(serde_json::Value::Object(map))
        }
        toml::Value::Array(items) => {
            let mut array = Vec::with_capacity(items.len());
            for item in items {
                array.push(theme_toml_to_json(item)?);
            }
            Ok(serde_json::Value::Array(array))
        }
        toml::Value::String(text) => Ok(serde_json::Value::String(text.clone())),
        toml::Value::Integer(int) => Ok(serde_json::Value::from(*int)),
        toml::Value::Float(float) => Ok(serde_json::Number::from_f64(*float)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)),
        toml::Value::Boolean(flag) => Ok(serde_json::Value::Bool(*flag)),
        toml::Value::Datetime(_) => Err("the theme must not contain dates".to_string()),
    }
}

fn backup_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "config.toml".to_string());
    path.with_file_name(format!("{name}.old"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("turya-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn defaults_apply_when_no_file_exists() {
        let path = tmp("missing");
        let (cfg, outcome) = TuryaConfig::load(&path);
        assert_eq!(outcome, None);
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert_eq!(cfg.auto_compact, Some(true));
        assert_eq!(cfg.keep_tokens, Some(15_000));
    }

    #[test]
    fn round_trips_through_disk() {
        let path = tmp("round");
        let theme = theme_json_to_toml(&serde_json::json!({
            "pack": "default",
            "overrides": { "tool": { "fg": "mustard" } }
        }))
        .expect("theme");
        let cfg = TuryaConfig {
            provider: Some("gemini".to_string()),
            model: Some("gemini-flash-lite-latest".to_string()),
            max_steps: Some(12),
            theme: Some(theme),
            ..TuryaConfig::default()
        };
        cfg.save(&path).expect("save");

        let (loaded, outcome) = TuryaConfig::load(&path);
        assert_eq!(outcome, Some(LoadOutcome::Loaded));
        assert_eq!(loaded.provider.as_deref(), Some("gemini"));
        assert_eq!(loaded.model.as_deref(), Some("gemini-flash-lite-latest"));
        assert_eq!(loaded.max_steps, Some(12));
        // The theme survives as the table the UI plugin handed over, and it
        // is still readable by the UI plugin.
        let stored = loaded.theme.expect("theme");
        let back = theme_toml_to_json(&stored).expect("json");
        assert_eq!(back["pack"], serde_json::json!("default"));
        assert_eq!(
            back["overrides"]["tool"]["fg"],
            serde_json::json!("mustard")
        );
    }

    #[test]
    fn the_theme_is_stored_as_a_readable_table() {
        let path = tmp("theme-table");
        let theme = theme_json_to_toml(&serde_json::json!({
            "pack": "default",
            "overrides": { "tool": { "fg": "mustard" } }
        }))
        .expect("theme");
        let cfg = TuryaConfig {
            theme: Some(theme),
            ..TuryaConfig::default()
        };
        cfg.save(&path).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(raw.contains("[theme]"), "{raw}");
        assert!(raw.contains("pack = \"default\""), "{raw}");
        assert!(raw.contains("fg = \"mustard\""), "{raw}");
    }

    #[test]
    fn a_theme_the_settings_file_cannot_hold_is_refused() {
        let empty = theme_json_to_toml(&serde_json::json!({ "pack": serde_json::Value::Null }))
            .expect_err("nulls cannot be stored");
        assert!(empty.contains("empty values"), "{empty}");
    }

    #[test]
    fn a_future_version_is_replaced_not_migrated() {
        let path = tmp("future");
        std::fs::write(&path, "version = 99\nprovider = \"x\"\n").unwrap();
        let (cfg, outcome) = TuryaConfig::load(&path);
        assert!(
            matches!(outcome, Some(LoadOutcome::Replaced { found: 99, .. })),
            "{outcome:?}"
        );
        assert_eq!(cfg.provider, None, "old values are not carried over");
        assert!(
            path.with_file_name("config.toml.old").exists(),
            "the old file is preserved"
        );
    }

    #[test]
    fn an_unversioned_file_is_replaced() {
        let path = tmp("unversioned");
        std::fs::write(&path, "provider = \"gemini\"\n").unwrap();
        let (_, outcome) = TuryaConfig::load(&path);
        assert!(
            matches!(outcome, Some(LoadOutcome::Replaced { found: 0, .. })),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_corrupt_file_is_replaced_with_a_reason() {
        let path = tmp("corrupt");
        std::fs::write(&path, "this is not = = toml [[[").unwrap();
        let (cfg, outcome) = TuryaConfig::load(&path);
        match outcome {
            Some(LoadOutcome::ReplacedUnparsable { reason, .. }) => {
                assert!(!reason.is_empty(), "the user is told why")
            }
            other => panic!("expected a replacement, got {other:?}"),
        }
        assert_eq!(cfg.version, CONFIG_VERSION);
    }

    #[test]
    fn unknown_keys_are_ignored_not_fatal() {
        let path = tmp("unknown");
        std::fs::write(
            &path,
            "version = 1\nprovider = \"gemini\"\nfuture_knob = 42\n",
        )
        .unwrap();
        let (cfg, outcome) = TuryaConfig::load(&path);
        assert_eq!(outcome, Some(LoadOutcome::Loaded));
        assert_eq!(cfg.provider.as_deref(), Some("gemini"));
    }

    #[test]
    fn a_missing_key_falls_back_to_the_default() {
        let path = tmp("sparse");
        std::fs::write(&path, "version = 1\n").unwrap();
        let (cfg, _) = TuryaConfig::load(&path);
        assert_eq!(cfg.keep_tokens, Some(15_000));
        assert_eq!(cfg.mouse.as_deref(), Some("auto"));
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let path = tmp("atomic");
        TuryaConfig::default().save(&path).unwrap();
        let tmp_left = path.with_extension("toml.tmp");
        assert!(!tmp_left.exists(), "no temp file left behind");
        assert!(path.exists());
    }
}
