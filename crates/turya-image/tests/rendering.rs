//! The contract the TUI depends on: a path in, either an escape sequence or a
//! sentence the user can act on. Never silence.

use std::path::{Path, PathBuf};
use turya_image::{render, Capability, Graphics, Skip};

fn png(dir: &Path, w: u32, h: u32) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("shot.png");
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&w.to_be_bytes());
    bytes.extend_from_slice(&h.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    std::fs::write(&path, &bytes).unwrap();
    path
}

fn cap(graphics: Option<Graphics>, interactive: bool) -> Capability {
    Capability {
        graphics,
        interactive,
    }
}

#[test]
fn kitty_output_is_a_path_not_a_payload() {
    let path = png(&std::env::temp_dir().join("turya-img-it"), 40, 30);
    let out = render(&cap(Some(Graphics::Kitty), true), &path).unwrap();
    // The terminal reads the file itself, so the bytes never cross the
    // process. That is the whole reason this crate has no image dependency.
    assert!(out.contains("a=T"), "{out:?}");
    assert!(out.contains(&path.to_string_lossy().to_string()), "{out:?}");
    assert!(
        !out.contains("iVBOR"),
        "no base64 payload expected: {out:?}"
    );
    assert!(out.ends_with("\n"), "must be followed by a newline");
}

#[test]
fn iterm_output_is_base64_because_it_has_no_path_form() {
    let path = png(&std::env::temp_dir().join("turya-img-it2"), 40, 30);
    let out = render(&cap(Some(Graphics::Iterm2), true), &path).unwrap();
    assert!(out.starts_with("\x1b]1337;File=inline=1"), "{out:?}");
    assert!(out.contains("iVBOR"), "iTerm needs the bytes: {out:?}");
    assert!(out.ends_with("\x07\n"), "{out:?}");
}

#[test]
fn a_terminal_without_graphics_support_is_told_so() {
    let path = png(&std::env::temp_dir().join("turya-img-none"), 10, 10);
    assert_eq!(
        render(&cap(None, true), &path),
        Err(Skip::NoTerminalSupport)
    );
    // And the message the TUI shows names the reason.
    assert_eq!(
        Skip::NoTerminalSupport.to_string(),
        "this terminal cannot display images inline"
    );
}

#[test]
fn a_redirected_stdout_gets_no_escapes_even_on_kitty() {
    let path = png(&std::env::temp_dir().join("turya-img-tty"), 10, 10);
    assert_eq!(
        render(&cap(Some(Graphics::Kitty), false), &path),
        Err(Skip::NoTerminalSupport),
        "a pipe must never receive escape sequences"
    );
}

#[test]
fn a_missing_file_does_not_look_like_an_unsupported_terminal() {
    let missing = std::env::temp_dir().join("turya-img-it/definitely-gone.png");
    assert_eq!(
        render(&cap(Some(Graphics::Kitty), true), &missing),
        Err(Skip::Missing)
    );
}

#[test]
fn every_skip_reason_reads_as_a_sentence() {
    for skip in [
        Skip::NotAnImage,
        Skip::Missing,
        Skip::TooLarge { bytes: 9_000_000 },
        Skip::TooManyPixels { pixels: 12_000_000 },
        Skip::NoTerminalSupport,
        Skip::Unreadable,
    ] {
        // A user-facing string, so it has to read as English rather than as a
        // debug name; every reason ends mid-sentence otherwise.
        let text = skip.to_string();
        assert!(text.ends_with(['y', 'd', 'e', 't']), "{skip:?} -> {text}");
    }
}
