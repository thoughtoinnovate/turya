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
    /// MCP servers enabled by id.
    pub mcp_enabled: Option<Vec<String>>,
    /// Extra skill directories.
    pub skills_paths: Option<Vec<String>>,
    /// Optional background tints, hex or `none`. Default: none, because a
    /// hard-pinned background is invisible on some themes and hostile on
    /// others. Identity is carried by the gutter glyph, not a wash.
    pub user_bg: Option<String>,
    pub assistant_bg: Option<String>,
    pub tool_bg: Option<String>,
    /// Set when the user asks for no colour at all (`NO_COLOR`).
    pub no_color: Option<bool>,
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
            auto_compact: Some(true),
            keep_tokens: Some(15_000),
            queue_behavior: Some("steer".to_string()),
            show_thinking: Some(true),
            mouse: Some("auto".to_string()),
            mcp_enabled: None,
            skills_paths: None,
            user_bg: None,
            assistant_bg: None,
            tool_bg: None,
            no_color: Some(false),
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
        let cfg = TuryaConfig {
            provider: Some("gemini".to_string()),
            model: Some("gemini-flash-lite-latest".to_string()),
            max_steps: Some(12),
            user_bg: Some("#1b2735".to_string()),
            ..TuryaConfig::default()
        };
        cfg.save(&path).expect("save");

        let (loaded, outcome) = TuryaConfig::load(&path);
        assert_eq!(outcome, Some(LoadOutcome::Loaded));
        assert_eq!(loaded.provider.as_deref(), Some("gemini"));
        assert_eq!(loaded.model.as_deref(), Some("gemini-flash-lite-latest"));
        assert_eq!(loaded.max_steps, Some(12));
        assert_eq!(loaded.user_bg.as_deref(), Some("#1b2735"));
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
