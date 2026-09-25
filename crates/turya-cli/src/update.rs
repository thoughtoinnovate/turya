//! Self-update for the turya binary (`turya update` / `turya upgrade`).
//!
//! Tark-style hand-rolled updater: resolves the target version (explicit pin
//! or latest GitHub release), compares it against the running binary, and
//! replaces the executable in place with SHA256 verification and a backup
//! for rollback. Mirrors `install.sh` so both entry points agree.
//!
//! Semantics: `update` applies patch/minor releases within the current major
//! and refuses to cross a major boundary (directs to `upgrade`); `upgrade`
//! may cross majors but requires explicit confirmation because of possible
//! breaking changes.

use std::io::Write;
use std::path::{Path, PathBuf};

/// GitHub repo hosting releases.
const RELEASE_REPO: &str = "thoughtoinnovate/turya";

/// Current binary version (bare number, e.g. `0.1.0`).
pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Strip a leading `v` (`v0.1.0` -> `0.1.0`).
pub fn normalize_version(input: &str) -> String {
    input.strip_prefix('v').unwrap_or(input).to_string()
}

/// Parse `major.minor.patch` (leading `v` tolerated).
pub fn parse_version(input: &str) -> Result<(u64, u64, u64), String> {
    let normalized = normalize_version(input);
    let parts: Vec<&str> = normalized.split('.').collect();
    if parts.len() != 3 {
        return Err(format!("Invalid version '{}': expected form v0.1.0", input));
    }
    let nums: Result<Vec<u64>, _> = parts.iter().map(|p| p.parse::<u64>()).collect();
    match nums {
        Ok(n) => Ok((n[0], n[1], n[2])),
        Err(_) => Err(format!("Invalid version '{}': non-numeric part", input)),
    }
}

/// Release asset name for this platform, mirroring release.yml artifacts.
/// Returns `None` on unsupported platforms instead of guessing.
pub fn asset_name() -> Option<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let base = match (os, arch) {
        ("linux", "x86_64") => "turya-linux-x86_64-musl",
        ("linux", "aarch64") => "turya-linux-arm64-musl",
        ("macos", "x86_64") => "turya-darwin-x86_64",
        ("macos", "aarch64") => "turya-darwin-arm64",
        ("windows", "x86_64") => "turya-windows-x86_64.exe",
        ("windows", "aarch64") => "turya-windows-arm64.exe",
        _ => return None,
    };
    Some(base.to_string())
}

/// Outcome of the version decision, kept pure for unit testing.
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateDecision {
    /// Proceed with download + replace.
    Proceed,
    /// Already on the target version.
    UpToDate,
    /// Target is older and no explicit pin/force was given.
    DowngradeBlocked,
    /// Target crosses a major boundary and majors are not allowed.
    RefuseMajor {
        current_major: u64,
        target_major: u64,
    },
}

/// Decide whether to proceed given parsed current/target versions.
/// `pinned` (explicit `--version`) counts as deliberate intent and bypasses
/// the up-to-date / downgrade / major guards, mirroring `install.sh`.
pub fn decide(
    current: (u64, u64, u64),
    target: (u64, u64, u64),
    allow_major: bool,
    force: bool,
    pinned: bool,
) -> UpdateDecision {
    if force || pinned {
        return UpdateDecision::Proceed;
    }
    match current.cmp(&target) {
        std::cmp::Ordering::Equal => UpdateDecision::UpToDate,
        std::cmp::Ordering::Greater => UpdateDecision::DowngradeBlocked,
        std::cmp::Ordering::Less => {
            if target.0 > current.0 && !allow_major {
                UpdateDecision::RefuseMajor {
                    current_major: current.0,
                    target_major: target.0,
                }
            } else {
                UpdateDecision::Proceed
            }
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct ReleaseInfo {
    #[serde(default)]
    tag_name: String,
}

/// Fetch the latest release tag (e.g. `v0.1.0`) from the GitHub API.
/// Note: `/releases/latest` excludes prereleases (including `nightly`),
/// so stable users are never auto-moved to nightly builds.
pub async fn fetch_latest_tag() -> Result<String, String> {
    let token = std::env::var("GITHUB_TOKEN").ok();
    let mut req = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("turya-self-update")
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?
        .get(format!(
            "https://api.github.com/repos/{RELEASE_REPO}/releases/latest"
        ));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let info: ReleaseInfo = req
        .send()
        .await
        .map_err(|e| format!("Failed to query latest release: {}", e))?
        .error_for_status()
        .map_err(|e| {
            format!(
                "GitHub API rejected the latest-release query (rate-limited? set GITHUB_TOKEN): {}",
                e
            )
        })?
        .json()
        .await
        .map_err(|e| format!("Failed to parse latest-release response: {}", e))?;
    parse_version(&info.tag_name)?;
    Ok(info.tag_name)
}

/// Download a release asset (plus its `.sha256` sidecar) into a temp dir.
/// Returns the verified binary path.
async fn download_verified(tag: &str, asset: &str, dir: &Path) -> Result<PathBuf, String> {
    let token = std::env::var("GITHUB_TOKEN").ok();
    let build_client = || {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .user_agent("turya-self-update")
            .build()
    };
    let get = |url: String| {
        let token = token.clone();
        async move {
            let client = build_client().map_err(|e| e.to_string())?;
            let mut req = client.get(url);
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            let bytes = req
                .send()
                .await
                .map_err(|e| e.to_string())?
                .error_for_status()
                .map_err(|e| e.to_string())?
                .bytes()
                .await
                .map_err(|e| e.to_string())?;
            Ok::<_, String>(bytes)
        }
    };

    let base = format!("https://github.com/{RELEASE_REPO}/releases/download/{tag}");
    let raw = get(format!("{base}/{asset}.sha256")).await?;
    let expected_hex = String::from_utf8_lossy(&raw)
        .split_whitespace()
        .next()
        .ok_or_else(|| "Empty checksum file".to_string())?
        .to_string();
    let bytes = get(format!("{base}/{asset}")).await?;
    verify_sha256(&bytes, &expected_hex)?;
    let bin_path = dir.join(asset);
    std::fs::write(&bin_path, &bytes)
        .map_err(|e| format!("Failed to stage {}: {}", bin_path.display(), e))?;
    Ok(bin_path)
}

/// Verify `bytes` against a hex SHA256 digest (case-insensitive).
pub fn verify_sha256(bytes: &[u8], expected_hex: &str) -> Result<(), String> {
    use sha2::Digest;
    let actual = hex::encode(sha2::Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected_hex.trim()) {
        Ok(())
    } else {
        Err("Checksum mismatch: binary failed SHA256 verification".to_string())
    }
}

/// Replace the running binary with `staged`, keeping a `.bak` copy.
/// Returns `(backup, installed)`. On failure the backup is restored when
/// possible. On Windows a running image cannot be overwritten, so the
/// current binary is renamed aside first (rename is allowed, delete is not).
fn replace_current_exe(staged: &Path) -> Result<(PathBuf, PathBuf), String> {
    let current =
        std::env::current_exe().map_err(|e| format!("Cannot locate running binary: {}", e))?;
    let backup = current.with_extension("bak");
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(&current, &backup)
        .map_err(|e| format!("Failed to stage backup of {}: {}", current.display(), e))?;
    if let Err(e) = std::fs::rename(staged, &current) {
        let _ = std::fs::rename(&backup, &current);
        return Err(format!("Failed to install {}: {}", current.display(), e));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&current)
            .map_err(|e| e.to_string())?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&current, perms).map_err(|e| e.to_string())?;
    }
    Ok((backup, current))
}

fn confirm_major_upgrade(current: &str, target: &str) -> Result<bool, String> {
    println!(
        "turya upgrade crosses a major version ({} -> {}).\n\
         Breaking changes are possible. Check the release notes first.\n\
         Proceed? [y/N]",
        current, target
    );
    print!("> ");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
}

async fn run(
    pinned: Option<&str>,
    force: bool,
    check_only: bool,
    allow_major: bool,
    auto_yes: bool,
    cmd_name: &str,
) -> Result<(), String> {
    let target = match pinned {
        Some(v) => {
            parse_version(v)?;
            v.to_string()
        }
        None => fetch_latest_tag().await?,
    };
    let current = current_version();
    let target_v = parse_version(&target)?;
    let current_v = parse_version(&current).map_err(|e| format!("Running binary reports {}", e))?;

    if !force {
        match decide(current_v, target_v, allow_major, force, pinned.is_some()) {
            UpdateDecision::UpToDate => {
                println!("turya {current} is already up to date (latest: {target}).");
                return Ok(());
            }
            UpdateDecision::DowngradeBlocked => {
                println!(
                    "Installed turya {current} is newer than latest release {target}; leaving it in place. Use --force to reinstall."
                );
                return Ok(());
            }
            UpdateDecision::RefuseMajor { .. } => {
                return Err(format!(
                    "Latest release {target} crosses a major version from {current}. \
                     Run `turya upgrade` to accept possible breaking changes, \
                     or pin explicitly with --version."
                ));
            }
            UpdateDecision::Proceed => {}
        }
    }
    if check_only {
        println!(
            "Update available: turya {current} -> {target}. Run `turya {cmd_name}` to install."
        );
        return Ok(());
    }

    if target_v.0 > current_v.0 && !auto_yes && !force && !confirm_major_upgrade(&current, &target)?
    {
        println!("Upgrade cancelled.");
        return Ok(());
    }

    let asset =
        asset_name().ok_or_else(|| "Self-update is not supported on this platform".to_string())?;
    println!("Updating turya {current} -> {target} ({asset})...");
    let dir = tempfile::tempdir().map_err(|e| format!("Failed to create staging dir: {}", e))?;
    let staged = download_verified(&target, &asset, dir.path()).await?;
    let (backup, installed) = replace_current_exe(&staged).map_err(|e| {
        format!(
            "{}. (Installed somewhere sudo-free like ~/.cargo/bin? Re-run install.sh with --install-dir.)",
            e
        )
    })?;
    println!(
        "turya updated to {target}. Backup kept at {}.",
        backup.display()
    );
    let reported: String = std::process::Command::new(&installed)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    if !reported.trim().is_empty() {
        println!("Reported version: {}", reported.trim());
    }
    Ok(())
}

/// Run `turya update`: patch/minor only, never crosses majors unpinned.
pub async fn run_self_update(
    pinned: Option<&str>,
    force: bool,
    check_only: bool,
) -> Result<(), String> {
    run(pinned, force, check_only, false, false, "update").await
}

/// Run `turya upgrade`: may cross major versions (with confirmation).
pub async fn run_upgrade(
    pinned: Option<&str>,
    force: bool,
    check_only: bool,
    yes: bool,
) -> Result<(), String> {
    run(pinned, force, check_only, true, yes, "upgrade").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse() {
        assert_eq!(parse_version("v0.1.0").unwrap(), (0, 1, 0));
        assert_eq!(parse_version("0.1.0").unwrap(), (0, 1, 0));
        assert!(parse_version("0.1").is_err());
        assert!(parse_version("abc").is_err());
        // Ordering across minor/major boundaries is covered by
        // update_refuses_major_upgrade_accepts_minor via decide().
    }

    #[test]
    fn update_refuses_major_upgrade_accepts_minor() {
        // update (allow_major=false): minor proceeds, major refused.
        assert_eq!(
            decide((0, 1, 0), (0, 1, 5), false, false, false),
            UpdateDecision::Proceed
        );
        assert_eq!(
            decide((0, 1, 0), (1, 0, 0), false, false, false),
            UpdateDecision::RefuseMajor {
                current_major: 0,
                target_major: 1
            }
        );
        // upgrade (allow_major=true): major proceeds.
        assert_eq!(
            decide((0, 1, 0), (1, 0, 0), true, false, false),
            UpdateDecision::Proceed
        );
        // Up-to-date and downgrade guards.
        assert_eq!(
            decide((0, 1, 0), (0, 1, 0), false, false, false),
            UpdateDecision::UpToDate
        );
        assert_eq!(
            decide((0, 2, 0), (0, 1, 9), false, false, false),
            UpdateDecision::DowngradeBlocked
        );
        // Explicit pin / force bypass all guards.
        assert_eq!(
            decide((0, 2, 0), (0, 1, 9), false, false, true),
            UpdateDecision::Proceed
        );
        assert_eq!(
            decide((0, 1, 0), (0, 1, 0), false, true, false),
            UpdateDecision::Proceed
        );
    }

    #[test]
    fn checksum_verification() {
        // SHA256("abc").
        let digest = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_sha256(b"abc", digest).is_ok());
        assert!(verify_sha256(b"abc", &digest.to_uppercase()).is_ok());
        assert!(verify_sha256(b"abd", digest).is_err());
        assert!(verify_sha256(b"", digest).is_err());
    }

    #[test]
    fn asset_name_matches_release_artifacts() {
        // Must mirror the artifact names published by release.yml
        // and the mapping in install.sh.
        if let Some(name) = asset_name() {
            assert!(name.starts_with("turya-"));
            #[cfg(windows)]
            assert!(name.ends_with(".exe"));
            #[cfg(not(windows))]
            assert!(!name.ends_with(".exe"));
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            assert_eq!(name, "turya-linux-x86_64-musl");
        }
    }

    #[test]
    fn current_version_is_semver() {
        assert!(parse_version(&current_version()).is_ok());
    }
}
