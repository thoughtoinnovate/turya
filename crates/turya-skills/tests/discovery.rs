//! Skills end to end: discovered on disk, advertised to the model, loaded on
//! demand.
//!
//! The assertion that matters is that the catalog text appears in what the
//! provider is actually handed. A hook that is wired but never called looks
//! identical to one that works, so we read the recorded transcript rather than
//! trusting the wiring.

use std::sync::{Arc, Mutex};

use turya_core::{LlmProvider, ProviderStep, SkillHook, TuryaEngine};
use turya_protocol::{AgentMode, Part, PermissionMode, Transcript};
use turya_skills::FileSkillProvider;
use turya_tools::ToolRegistry;

/// Records every transcript it is handed, so a test can see the request.
#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<Transcript>>,
}

#[async_trait::async_trait]
impl LlmProvider for Recorder {
    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tx: tokio::sync::mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        self.seen.lock().unwrap().push(transcript.clone());
        let _ = tx.send(ProviderStep::Finish).await;
        Ok(())
    }
}

fn write_skill(root: &std::path::Path, dir: &str, name: &str, description: &str, body: &str) {
    let d = root.join(dir);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}"),
    )
    .unwrap();
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("turya-skills-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn run(engine: &Arc<TuryaEngine>, prompt: &str) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let (_p, prx) = tokio::sync::mpsc::channel(4);
    engine
        .run_turn("t1", prompt, AgentMode::Build, &[], tx, prx)
        .await;
    while rx.recv().await.is_some() {}
}

fn instructions_in(t: &Transcript) -> Vec<String> {
    t.turns
        .iter()
        .flat_map(|turn| turn.parts.iter())
        .filter_map(|p| match p {
            Part::Instruction { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_catalog_reaches_the_model_and_the_body_loads_on_demand() {
    let root = tmp("catalog");
    write_skill(
        &root,
        "release-notes",
        "release-notes",
        "Draft release notes from commits",
        "Run `git log` and summarise.\n",
    );

    let skills = FileSkillProvider::new(&[root.to_string_lossy().to_string()]);
    let recorder = Arc::new(Recorder::default());
    let engine = Arc::new(
        TuryaEngine::new(
            recorder.clone(),
            Arc::new(ToolRegistry::standard()),
            PermissionMode::Open,
        )
        .with_skills_hook(Arc::new(skills))
        .with_session_id("s1"),
    );

    run(&engine, "write the release notes").await;

    // What the provider actually received must contain the catalog.
    let (call_count, instructions) = {
        let seen = recorder.seen.lock().unwrap();
        (seen.len(), instructions_in(&seen[0]))
    };
    assert_eq!(call_count, 1, "one model call");
    assert_eq!(instructions.len(), 1, "{instructions:?}");
    let catalog = instructions[0].clone();
    assert!(
        catalog.contains("release-notes"),
        "the skill is advertised: {catalog}"
    );
    assert!(
        catalog.contains("Draft release notes from commits"),
        "with its description, which is what the model matches on: {catalog}"
    );
    assert!(
        catalog.contains("SKILL.md"),
        "and its location, so activation needs no new machinery: {catalog}"
    );
    // The body is NOT in the request: that is the point of progressive
    // disclosure, and it is asserted so nobody "helpfully" inlines it.
    assert!(
        !catalog.contains("git log"),
        "the body stays out of context until loaded: {catalog}"
    );

    let body = engine
        .load_skill_body("release-notes")
        .await
        .expect("loads");
    assert!(body.contains("Run `git log`"), "{body}");
    assert!(
        engine.load_skill_body("nope").await.is_none(),
        "a guessed skill is not an error"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_turn_with_no_skills_injects_nothing() {
    // An empty catalog must not leave an empty instruction block in the
    // prompt: that is noise the model reads on every single turn.
    let root = tmp("empty");
    let skills = FileSkillProvider::new(&[root.to_string_lossy().to_string()]);
    let recorder = Arc::new(Recorder::default());
    let engine = Arc::new(
        TuryaEngine::new(
            recorder.clone(),
            Arc::new(ToolRegistry::standard()),
            PermissionMode::Open,
        )
        .with_skills_hook(Arc::new(skills))
        .with_session_id("s1"),
    );
    run(&engine, "hello").await;

    let seen = recorder.seen.lock().unwrap();
    assert!(
        instructions_in(&seen[0]).is_empty(),
        "no skills, no prompt noise"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_broken_skill_is_skipped_and_the_good_one_still_loads() {
    // One malformed skill must not take the rest of the session's tools away.
    let root = tmp("broken");
    write_skill(&root, "good", "good", "works", "body\n");
    let bad = root.join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(
        bad.join("SKILL.md"),
        "---\nname: bad\n---\nno description\n",
    )
    .unwrap();

    let skills = FileSkillProvider::new(&[root.to_string_lossy().to_string()]);
    let catalog = skills.skill_catalog("s1").await;
    assert_eq!(catalog.len(), 1, "only the usable skill: {catalog:?}");
    assert_eq!(catalog[0].name, "good");
    let warnings = skills.warnings();
    assert_eq!(
        warnings.len(),
        1,
        "and the problem is reported: {warnings:?}"
    );
    assert!(warnings[0].contains("description"), "{warnings:?}");
    let _ = std::fs::remove_dir_all(&root);
}
