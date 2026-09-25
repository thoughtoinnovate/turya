use turya_lsp::{LspBridge, Severity};

/// Live end-to-end check against a real `rust-analyzer` on PATH.
///
/// Ignored by default so `make test` stays fast and deterministic.
/// Run explicitly with: `cargo test -p turya-lsp --test live_probe -- --ignored --nocapture`
#[tokio::test]
#[ignore]
async fn live_rust_analyzer_reports_broken_file() {
    let dir = std::env::temp_dir().join("turya-live-lsp-probe");
    std::fs::create_dir_all(&dir).unwrap();
    // Minimal crate so rust-analyzer attaches properly.
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let src_dir = dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let target = src_dir.join("main.rs");
    std::fs::write(&target, "fn main() {\n    let x: i32 = \"not a number\";\n}\n").unwrap();

    let bridge = LspBridge::rust_analyzer();
    let diags = bridge.diagnose_file(&target).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !diags.is_empty(),
        "expected rust-analyzer to report the type mismatch"
    );
    assert!(diags.iter().any(|d| d.severity == Severity::Error));
    assert!(diags.iter().any(|d| d.message.contains("i32")));
}
