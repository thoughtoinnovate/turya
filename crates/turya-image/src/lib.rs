//! Inline image display for the TUI (internal plugin).
//!
//! Scope is deliberately Kitty and iTerm2, and the reason is that both can
//! be handed a *file path*: the terminal reads and decodes the bytes itself.
//! That is why this crate needs no image library at all. Sixel is the format
//! the plan originally listed, but emitting it means decoding pixels here,
//! which means an inflate dependency and a decoder for every format the user
//! might attach. A sixel terminal gets the honest placeholder instead of a
//! half-working renderer.
//!
//! Every cap is enforced before a single byte is sent. Terminals have been
//! known to wedge on a large inline payload, and a wedged terminal is worse
//! than a missing picture.

use std::path::Path;

/// Largest file we will inline. 8 MB is well past any screenshot; past this
/// the honest placeholder is more useful than a stall.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Largest pixel count we will inline. Checked only for the formats whose
/// dimensions live in the first bytes, which is all we support.
pub const MAX_PIXELS: u64 = 8_000_000;

/// How the terminal wants images, if it wants them at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Graphics {
    /// Kitty graphics protocol, including the terminals that speak it.
    Kitty,
    /// iTerm2's inline file protocol.
    Iterm2,
}

/// The user's override. `Auto` is the default because a terminal that lies
/// about its identity should be overridable in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Auto,
    /// Never emit image escapes, whatever the terminal claims.
    Off,
    /// Emit them even if detection said no. For testing and for terminals
    /// that do not identify themselves.
    Force,
}

impl Mode {
    pub fn parse(raw: &str) -> Option<Mode> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Mode::Auto),
            "off" | "never" | "0" => Some(Mode::Off),
            "force" | "on" | "always" | "1" => Some(Mode::Force),
            _ => None,
        }
    }
}

/// What this terminal can display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capability {
    pub graphics: Option<Graphics>,
    /// True when the escapes would be written to a real terminal. A pipe or
    /// a log file must never receive escape sequences.
    pub interactive: bool,
}

impl Capability {
    pub fn can_display_images(&self) -> bool {
        self.interactive && self.graphics.is_some()
    }
}

/// Detect from the environment. Takes the environment as arguments so the
/// whole matrix is testable without mutating the process.
pub fn detect<F>(mode: Mode, get: F, is_tty: bool) -> Capability
where
    F: Fn(&str) -> Option<String>,
{
    let interactive = is_tty;
    let graphics = match mode {
        Mode::Off => None,
        Mode::Force => Some(Graphics::Kitty),
        Mode::Auto => detect_auto(&get),
    };
    Capability {
        graphics,
        interactive,
    }
}

fn detect_auto<F>(get: &F) -> Option<Graphics>
where
    F: Fn(&str) -> Option<String>,
{
    let term = get("TERM").unwrap_or_default();
    let program = get("TERM_PROGRAM").unwrap_or_default();
    let lc = get("LC_TERMINAL").unwrap_or_default();

    // Kitty and the terminals that implement its protocol.
    if term.contains("kitty")
        || get("KITTY_WINDOW_ID").is_some()
        || program == "ghostty"
        || program == "WezTerm"
        || program == "rio"
    {
        return Some(Graphics::Kitty);
    }
    if program == "iTerm.app" || lc == "iTerm2" || get("ITERM_SESSION_ID").is_some() {
        return Some(Graphics::Iterm2);
    }
    None
}

/// Why an image was not rendered, so the caller can say something useful
/// instead of silently dropping a file the user attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    NotAnImage,
    Missing,
    TooLarge { bytes: u64 },
    TooManyPixels { pixels: u64 },
    NoTerminalSupport,
    Unreadable,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Skip::NotAnImage => write!(f, "not an image"),
            Skip::Missing => write!(f, "file not found"),
            Skip::TooLarge { bytes } => {
                write!(
                    f,
                    "{:.1} MB exceeds the 8 MB inline limit",
                    *bytes as f64 / 1e6
                )
            }
            Skip::TooManyPixels { pixels } => {
                write!(f, "{pixels} pixels exceeds the 8 MP inline limit")
            }
            Skip::NoTerminalSupport => write!(f, "this terminal cannot display images inline"),
            Skip::Unreadable => write!(f, "could not read the file"),
        }
    }
}

/// Image formats the terminals decode for us. Both protocols hand the
/// terminal a path, so we only need to know what the terminal accepts.
fn mime_for(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// Pixel dimensions, read from the file header only.
///
/// The cap exists to protect the terminal, and the only cheap honest way to
/// enforce it is to read the header. Formats whose header we do not parse
/// return `None`, and the caller proceeds on the byte cap alone rather than
/// guessing.
pub fn pixel_count(path: &Path) -> Option<u64> {
    let head = read_head(path, 32)?;
    if head.starts_with(&[0x89, b'P', b'N', b'G']) && head.len() >= 24 {
        let w = u32::from_be_bytes([head[16], head[17], head[18], head[19]]) as u64;
        let h = u32::from_be_bytes([head[20], head[21], head[22], head[23]]) as u64;
        return Some(w * h);
    }
    if head.len() >= 4 && head[0] == 0xFF && head[1] == 0xD8 {
        // JPEG: walk the segment markers to the first SOFn.
        let mut i = 2usize;
        while i + 9 < head.len() {
            if head[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = head[i + 1];
            // SOF0..SOF15, excluding the non-frame markers in that range.
            if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC
            {
                let h = u16::from_be_bytes([head[i + 5], head[i + 6]]) as u64;
                let w = u16::from_be_bytes([head[i + 7], head[i + 8]]) as u64;
                return Some(w * h);
            }
            let len = u16::from_be_bytes([head[i + 2], head[i + 3]]) as usize;
            if len < 2 {
                return None;
            }
            i += 2 + len;
        }
    }
    None
}

fn read_head(path: &Path, n: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; n];
    let read = f.read(&mut buf).ok()?;
    buf.truncate(read);
    Some(buf)
}

/// The escape sequence that displays `path` inline, or why not.
///
/// Kitty is sent the path itself (`a=T`, transmit-by-file), so the bytes
/// never pass through this process. iTerm2 has no path form, so the file is
/// read and base64'd here - which is why the byte cap is checked first.
pub fn render(cap: &Capability, path: &Path) -> Result<String, Skip> {
    let graphics = cap.graphics.ok_or(Skip::NoTerminalSupport)?;
    if !cap.interactive {
        return Err(Skip::NoTerminalSupport);
    }
    let _mime = mime_for(path).ok_or(Skip::NotAnImage)?;
    let meta = std::fs::metadata(path).map_err(|_| Skip::Missing)?;
    if meta.len() > MAX_BYTES {
        return Err(Skip::TooLarge { bytes: meta.len() });
    }
    if let Some(pixels) = pixel_count(path) {
        if pixels > MAX_PIXELS {
            return Err(Skip::TooManyPixels { pixels });
        }
    }

    match graphics {
        Graphics::Kitty => {
            // a=T transmit by path, f=100 payload is the file, t=d direct
            // placement. The trailing m=1 clears the placeholder block the
            // terminal shows while the file is being read.
            Ok(format!(
                "\x1b_Ga=T,f=100,t=d;{}\x1b\\\n\x1b_Gm=1\x1b\\\n",
                path.to_string_lossy()
            ))
        }
        Graphics::Iterm2 => {
            let bytes = std::fs::read(path).map_err(|_| Skip::Unreadable)?;
            Ok(format!(
                "\x1b]1337;File=inline=1;width=auto;height=auto;preserveAspectRatio=1:{}\x07\n",
                base64(&bytes)
            ))
        }
    }
}

/// Standard base64, written out rather than pulled in: it is forty lines and
/// the only user is one escape sequence.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn kitty_is_detected_from_term_and_from_the_window_id() {
        let by_term = detect(Mode::Auto, env(&[("TERM", "xterm-kitty")]), true);
        assert_eq!(by_term.graphics, Some(Graphics::Kitty));
        let by_id = detect(Mode::Auto, env(&[("KITTY_WINDOW_ID", "1")]), true);
        assert_eq!(by_id.graphics, Some(Graphics::Kitty));
    }

    #[test]
    fn the_terminals_that_speak_kitty_are_recognised() {
        for program in ["ghostty", "WezTerm", "rio"] {
            let c = detect(Mode::Auto, env(&[("TERM_PROGRAM", program)]), true);
            assert_eq!(c.graphics, Some(Graphics::Kitty), "{program}");
        }
    }

    #[test]
    fn iterm_is_detected_but_a_plain_xterm_is_not() {
        let it = detect(Mode::Auto, env(&[("TERM_PROGRAM", "iTerm.app")]), true);
        assert_eq!(it.graphics, Some(Graphics::Iterm2));
        let xterm = detect(Mode::Auto, env(&[("TERM", "xterm-256color")]), true);
        assert_eq!(xterm.graphics, None);
    }

    #[test]
    fn nothing_is_emitted_when_stdout_is_not_a_terminal() {
        // The important case: a pipe or a log file must never see escapes.
        let c = detect(Mode::Auto, env(&[("TERM", "xterm-kitty")]), false);
        assert_eq!(c.graphics, Some(Graphics::Kitty));
        assert!(!c.can_display_images());
    }

    #[test]
    fn off_beats_detection_and_force_works_on_any_terminal() {
        let off = detect(Mode::Off, env(&[("TERM", "xterm-kitty")]), true);
        assert_eq!(off.graphics, None);
        let forced = detect(Mode::Force, env(&[("TERM", "dumb")]), true);
        assert_eq!(forced.graphics, Some(Graphics::Kitty));
    }

    #[test]
    fn mode_parsing_rejects_nonsense_instead_of_defaulting() {
        assert_eq!(Mode::parse("auto"), Some(Mode::Auto));
        assert_eq!(Mode::parse("OFF"), Some(Mode::Off));
        assert_eq!(Mode::parse("force"), Some(Mode::Force));
        assert_eq!(Mode::parse("sometimes"), None);
    }

    #[test]
    fn non_images_are_refused_by_extension() {
        let dir = std::env::temp_dir().join("turya-img-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.txt");
        std::fs::write(&path, "hello").unwrap();
        let cap = Capability {
            graphics: Some(Graphics::Kitty),
            interactive: true,
        };
        assert_eq!(render(&cap, &path), Err(Skip::NotAnImage));
    }

    #[test]
    fn an_oversized_file_is_refused_before_being_read() {
        let dir = std::env::temp_dir().join("turya-img-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.png");
        // A sparse file: the cap must trip on metadata, not on a read.
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_BYTES + 1).unwrap();
        drop(f);
        let cap = Capability {
            graphics: Some(Graphics::Kitty),
            interactive: true,
        };
        assert!(matches!(render(&cap, &path), Err(Skip::TooLarge { .. })));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn base64_matches_the_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn png_dimensions_come_from_the_header_without_decoding_pixels() {
        let dir = std::env::temp_dir().join("turya-img-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.png");
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&4000u32.to_be_bytes());
        bytes.extend_from_slice(&3000u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(pixel_count(&path), Some(12_000_000));
        let cap = Capability {
            graphics: Some(Graphics::Kitty),
            interactive: true,
        };
        assert!(matches!(
            render(&cap, &path),
            Err(Skip::TooManyPixels { pixels: 12_000_000 })
        ));
    }
}
