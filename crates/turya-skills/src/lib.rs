//! Agent Skills discovery (internal plugin).
//!
//! Implements the open Agent Skills format: a skill is a directory holding a
//! `SKILL.md` with YAML frontmatter (`name`, `description` required) followed by
//! markdown instructions. Implements the *catalog* half of progressive
//! disclosure — name, description and location are advertised cheaply, and the
//! body is read on activation, so a project with fifty skills does not pay for
//! fifty bodies.
//!
//! Discovery scans `.agents/skills/` (the cross-client convention) plus any
//! configured paths, and recurses one level: a skills directory may hold
//! either skill folders or loose `SKILL.md` files.
//!
//! Frontmatter is parsed with a small, strict reader rather than a YAML
//! dependency: only the handful of scalar fields the spec defines are read, and
//! anything unexpected is skipped with a reason instead of being guessed at.

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use turya_core::{SkillHook, SkillRef};

/// How deep to look below each skills root. The spec keeps a skill's own
/// resources one level down; deeper nesting is a different project's business.
const MAX_DEPTH: usize = 3;

/// A discovered skill plus its parsed metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub location: PathBuf,
    /// `true` when the skill bundles `scripts/`, so a caller can advertise a
    /// script runner only when one actually exists.
    pub has_scripts: bool,
}

/// Filesystem skill provider. Discovery is lazy and cached: the catalog is
/// re-read only when a `refresh` is asked for, so a session does not stat the
/// same directories on every turn.
pub struct FileSkillProvider {
    roots: Vec<PathBuf>,
    cache: Mutex<Option<Vec<Skill>>>,
    /// Failures worth telling the user about, surfaced by `warnings()`.
    warnings: Mutex<Vec<String>>,
}

impl FileSkillProvider {
    /// Build a provider over the standard locations plus `extra`.
    pub fn new(extra: &[String]) -> Self {
        let mut roots = vec![
            PathBuf::from(".agents/skills"),
            PathBuf::from(".turya/skills"),
        ];
        for e in extra {
            roots.push(PathBuf::from(e));
        }
        Self {
            roots,
            cache: Mutex::new(None),
            warnings: Mutex::new(Vec::new()),
        }
    }

    /// Re-scan the roots, replacing the cache. Exposed so `/skills` can pick
    /// up a skill the user just added without restarting.
    pub fn refresh(&self) {
        *self.cache.lock().unwrap() = None;
        *self.warnings.lock().unwrap() = Vec::new();
    }

    /// Problems found during the last scan (a malformed skill, say).
    pub fn warnings(&self) -> Vec<String> {
        self.warnings.lock().unwrap().clone()
    }

    /// The catalog, scanning on first use.
    pub fn skills(&self) -> Vec<Skill> {
        if let Some(cached) = self.cache.lock().unwrap().as_ref() {
            return cached.clone();
        }
        let mut warnings = Vec::new();
        let mut found: Vec<Skill> = Vec::new();
        let mut seen: HashMap<String, PathBuf> = HashMap::new();
        for root in &self.roots {
            if !root.is_dir() {
                continue;
            }
            for (_path, maybe) in scan_root(root, 0) {
                match maybe {
                    Ok(skill) => {
                        // First match wins, so a project-local skill beats a
                        // user-global one with the same name.
                        if seen.contains_key(&skill.name) {
                            continue;
                        }
                        seen.insert(skill.name.clone(), skill.location.clone());
                        found.push(skill);
                    }
                    Err(e) => warnings.push(e),
                }
            }
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        *self.cache.lock().unwrap() = Some(found.clone());
        *self.warnings.lock().unwrap() = warnings;
        found
    }
}

/// Walk a skills root, yielding each `SKILL.md` with its parse outcome.
/// Errors are values, not panics: one bad skill must not hide the rest.
fn scan_root(root: &Path, depth: usize) -> Vec<(PathBuf, Result<Skill, String>)> {
    if depth > MAX_DEPTH {
        return Vec::new();
    }
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            let manifest = path.join("SKILL.md");
            if manifest.is_file() {
                out.push((manifest.clone(), parse_skill(&path, &manifest)));
            } else {
                out.extend(scan_root(&path, depth + 1));
            }
        }
    }
    out
}

/// Parse one skill directory. `name` falls back to the directory name, which
/// the spec also requires them to match — a mismatch is reported, not fatal.
fn parse_skill(dir: &Path, manifest: &Path) -> Result<Skill, String> {
    let text = std::fs::read_to_string(manifest)
        .map_err(|e| format!("{}: cannot read ({e})", manifest.display()))?;
    let (front, _body) = split_frontmatter(&text)
        .ok_or_else(|| format!("{}: no YAML frontmatter", manifest.display()))?;
    let fields = parse_scalar_frontmatter(front);
    let dir_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = fields
        .get("name")
        .cloned()
        .unwrap_or_else(|| dir_name.clone());
    let description = fields
        .get("description")
        .cloned()
        .ok_or_else(|| format!("{}: missing required 'description'", manifest.display()))?;
    Ok(Skill {
        name,
        description,
        location: manifest.to_path_buf(),
        has_scripts: dir.join("scripts").is_dir(),
    })
}

/// Split `---` delimited frontmatter from the markdown body.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    // Find the closing delimiter at the start of a line.
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed == "---" || trimmed == "..." {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// Read the scalar fields we care about.
///
/// Deliberately tiny: `key: value` lines, optionally quoted, with `#` comments
/// stripped. Multi-line YAML constructs are ignored rather than misread, so an
/// exotic frontmatter degrades to "field not found" instead of a wrong value.
fn parse_scalar_frontmatter(front: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.contains(' ') {
            continue;
        }
        let mut value = raw.trim();
        // Strip a trailing comment only when it cannot be inside quotes.
        if !value.starts_with('"') && !value.starts_with('\'') {
            if let Some(hash) = value.find(" #") {
                value = value[..hash].trim();
            }
        }
        let value = value.trim_matches(|c| c == '"' || c == '\'').trim();
        if !value.is_empty() {
            out.insert(key.to_string(), value.to_string());
        }
    }
    out
}

#[async_trait]
impl SkillHook for FileSkillProvider {
    async fn skill_catalog(&self, _session_id: &str) -> Vec<SkillRef> {
        self.skills()
            .into_iter()
            .map(|s| SkillRef {
                name: s.name,
                description: s.description,
                location: s.location.to_string_lossy().to_string(),
            })
            .collect()
    }

    async fn load_skill(&self, name: &str) -> Option<String> {
        let skill = self.skills().into_iter().find(|s| s.name == name)?;
        // The body is read at activation, not at discovery, so a skill the
        // user edits mid-session is picked up on the next activation.
        std::fs::read_to_string(&skill.location).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("turya-skills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(root: &Path, dir_name: &str, body: &str) {
        let d = root.join(dir_name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn finds_skills_and_parses_required_fields() {
        let root = tmp("basic");
        write_skill(
            &root,
            "pdf",
            "---\nname: pdf\ndescription: Extract text from PDFs\n---\n\nDo the thing.\n",
        );
        let p = FileSkillProvider::new(&[root.to_string_lossy().to_string()]);
        let skills = p.skills();
        assert_eq!(skills.len(), 1, "{skills:?}");
        assert_eq!(skills[0].name, "pdf");
        assert_eq!(skills[0].description, "Extract text from PDFs");
        assert!(p.warnings().is_empty(), "no complaints: {:?}", p.warnings());
    }

    #[test]
    fn a_skill_without_a_description_is_skipped_with_a_reason() {
        let root = tmp("nodesc");
        write_skill(&root, "broken", "---\nname: broken\n---\nbody\n");
        let p = FileSkillProvider::new(&[root.to_string_lossy().to_string()]);
        assert!(p.skills().is_empty(), "a nameless skill is not advertised");
        let w = p.warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("description"), "{w:?}");
    }

    #[test]
    fn a_skill_without_frontmatter_is_skipped() {
        let root = tmp("nofront");
        write_skill(&root, "raw", "just markdown, no frontmatter\n");
        let p = FileSkillProvider::new(&[root.to_string_lossy().to_string()]);
        assert!(p.skills().is_empty());
        assert!(p.warnings()[0].contains("frontmatter"));
    }

    #[test]
    fn a_malformed_skill_does_not_hide_the_good_ones() {
        let root = tmp("mixed");
        write_skill(
            &root,
            "aaa-good",
            "---\nname: aaa-good\ndescription: fine\n---\n",
        );
        write_skill(&root, "mmm-bad", "---\nname: mmm-bad\n---\n");
        write_skill(
            &root,
            "zzz-good",
            "---\nname: zzz-good\ndescription: also fine\n---\n",
        );
        let p = FileSkillProvider::new(&[root.to_string_lossy().to_string()]);
        let found = p.skills();
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["aaa-good", "zzz-good"]);
        assert_eq!(p.warnings().len(), 1);
    }

    #[test]
    fn scripts_are_detected_so_a_runner_is_only_advertised_when_real() {
        let root = tmp("scripts");
        write_skill(&root, "with", "---\nname: with\ndescription: d\n---\n");
        std::fs::create_dir_all(root.join("with/scripts")).unwrap();
        write_skill(
            &root,
            "without",
            "---\nname: without\ndescription: d\n---\n",
        );
        let p = FileSkillProvider::new(&[root.to_string_lossy().to_string()]);
        let skills = p.skills();
        assert!(
            skills
                .iter()
                .find(|s| s.name == "with")
                .unwrap()
                .has_scripts
        );
        assert!(
            !skills
                .iter()
                .find(|s| s.name == "without")
                .unwrap()
                .has_scripts
        );
    }

    #[test]
    fn frontmatter_parsing_handles_quotes_comments_and_junk() {
        let f = "name: \"quoted name\"\ndescription: 'single'\n# a comment\nempty:\nlist:\n  - a\n  - b\nbad line without colon\nkey with spaces: v\n";
        let fields = parse_scalar_frontmatter(f);
        assert_eq!(fields.get("name").unwrap(), "quoted name");
        assert_eq!(fields.get("description").unwrap(), "single");
        assert!(!fields.contains_key("empty"), "empty value is not a field");
        assert!(
            !fields.contains_key("list"),
            "multi-line construct is skipped"
        );
        assert!(!fields.contains_key("key with spaces"));
    }

    #[test]
    fn a_missing_root_is_not_an_error() {
        let p = FileSkillProvider::new(&["/definitely/not/here".to_string()]);
        assert!(p.skills().is_empty());
        assert!(
            p.warnings().is_empty(),
            "an absent directory is not a problem"
        );
    }

    #[test]
    fn discovery_caches_until_refreshed() {
        let root = tmp("cache");
        write_skill(&root, "one", "---\nname: one\ndescription: d\n---\n");
        let p = FileSkillProvider::new(&[root.to_string_lossy().to_string()]);
        assert_eq!(p.skills().len(), 1);
        write_skill(&root, "two", "---\nname: two\ndescription: d\n---\n");
        assert_eq!(p.skills().len(), 1, "cached until refreshed");
        p.refresh();
        assert_eq!(p.skills().len(), 2, "refresh re-reads the roots");
    }

    #[test]
    fn the_catalog_says_how_to_load_and_is_empty_when_there_is_nothing() {
        // The wording lives in the kernel seam (turya_core::render_catalog)
        // so a second backend cannot forget how to load a skill.
        use turya_core::skills_catalog as render_catalog;
        assert_eq!(render_catalog(&[]), "");
        let catalog = render_catalog(&[SkillRef {
            name: "pdf".to_string(),
            description: "Extract PDF text".to_string(),
            location: "/x/pdf/SKILL.md".to_string(),
        }]);
        assert!(catalog.contains("read that file"), "{catalog}");
        assert!(catalog.contains("pdf: Extract PDF text"), "{catalog}");
        assert!(catalog.contains("/x/pdf/SKILL.md"), "{catalog}");
    }

    #[tokio::test]
    async fn loading_a_skill_returns_its_body_and_a_guess_returns_none() {
        let root = tmp("load");
        write_skill(
            &root,
            "guide",
            "---\nname: guide\ndescription: d\n---\n\nFollow these steps.\n",
        );
        let p = FileSkillProvider::new(&[root.to_string_lossy().to_string()]);
        let body = p.load_skill("guide").await.expect("loads");
        assert!(body.contains("Follow these steps"), "{body}");
        assert!(
            p.load_skill("not-a-skill").await.is_none(),
            "a guess is not an error"
        );
    }
}
