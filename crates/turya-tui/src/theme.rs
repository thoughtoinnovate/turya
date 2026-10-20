//! The theme the transcript paints itself with.
//!
//! A theme is a *named pack* plus per-voice overrides. The pack supplies the
//! defaults; an override replaces one slot — a foreground or a background — of
//! one voice. Nothing else can change a colour: the kernel never sees a
//! colour, and the host stores a theme without interpreting it.
//!
//! The four voices are the roles a transcript row can have. Identity is
//! carried by the gutter glyph, never by colour alone, so every pack is a
//! readable terminal rather than only a pretty one.

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

/// The pack every theme starts from.
pub const DEFAULT_PACK: &str = "default";

/// The packs this build ships, with a one-line description for `/settings`.
pub const PACKS: &[(&str, &str)] = &[
    (
        "default",
        "Turya's own palette: cyan prompts, grey tool calls, dim toasts.",
    ),
    (
        "high-contrast",
        "Brighter voices for terminals that wash out the default palette.",
    ),
];

/// A transcript row's voice. Every row belongs to exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Voice {
    User,
    Assistant,
    Tool,
    System,
}

impl Voice {
    /// Every voice, in the order the settings help lists them.
    pub const ALL: [Voice; 4] = [Voice::User, Voice::Assistant, Voice::Tool, Voice::System];

    /// The name used in settings keys (`tool.fg`) and in the help text.
    pub fn name(self) -> &'static str {
        match self {
            Voice::User => "user",
            Voice::Assistant => "assistant",
            Voice::Tool => "tool",
            Voice::System => "system",
        }
    }

    /// The voice a settings key names, if it names one.
    pub fn parse(raw: &str) -> Option<Voice> {
        match raw.to_ascii_lowercase().as_str() {
            "user" => Some(Voice::User),
            "assistant" => Some(Voice::Assistant),
            "tool" => Some(Voice::Tool),
            "system" => Some(Voice::System),
            _ => None,
        }
    }
}

/// Which slot of a voice a setting changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Foreground,
    Background,
}

impl Slot {
    fn name(self) -> &'static str {
        match self {
            Slot::Foreground => "fg",
            Slot::Background => "bg",
        }
    }
}

/// Parse `<voice>.<slot>` into the slot it names.
pub fn parse_slot_key(key: &str) -> Option<(Voice, Slot)> {
    let (voice, slot) = key.split_once('.')?;
    let voice = Voice::parse(voice.trim())?;
    let slot = match slot.trim().to_ascii_lowercase().as_str() {
        "fg" | "foreground" => Slot::Foreground,
        "bg" | "background" => Slot::Background,
        _ => return None,
    };
    Some((voice, slot))
}

/// One slot's override: the raw colour the user asked for, kept as text so
/// the settings file stays readable and a name never degrades into a number.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Slots {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
}

impl Slots {
    pub fn is_empty(&self) -> bool {
        self.fg.is_none() && self.bg.is_none()
    }

    pub fn get(&self, slot: Slot) -> &Option<String> {
        match slot {
            Slot::Foreground => &self.fg,
            Slot::Background => &self.bg,
        }
    }

    pub fn set(&mut self, slot: Slot, value: Option<String>) {
        match slot {
            Slot::Foreground => self.fg = value,
            Slot::Background => self.bg = value,
        }
    }
}

/// The per-voice overrides of a theme.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Overrides {
    #[serde(default, skip_serializing_if = "Slots::is_empty")]
    pub user: Slots,
    #[serde(default, skip_serializing_if = "Slots::is_empty")]
    pub assistant: Slots,
    #[serde(default, skip_serializing_if = "Slots::is_empty")]
    pub tool: Slots,
    #[serde(default, skip_serializing_if = "Slots::is_empty")]
    pub system: Slots,
}

impl Overrides {
    pub fn slots(&self, voice: Voice) -> &Slots {
        match voice {
            Voice::User => &self.user,
            Voice::Assistant => &self.assistant,
            Voice::Tool => &self.tool,
            Voice::System => &self.system,
        }
    }

    pub fn slots_mut(&mut self, voice: Voice) -> &mut Slots {
        match voice {
            Voice::User => &mut self.user,
            Voice::Assistant => &mut self.assistant,
            Voice::Tool => &mut self.tool,
            Voice::System => &mut self.system,
        }
    }

    pub fn is_empty(&self) -> bool {
        Voice::ALL.iter().all(|v| self.slots(*v).is_empty())
    }
}

/// The theme itself: a pack plus the overrides applied on top of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Theme {
    /// The pack the overrides sit on.
    pub pack: String,
    #[serde(default, skip_serializing_if = "Overrides::is_empty")]
    pub overrides: Overrides,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            pack: DEFAULT_PACK.to_string(),
            overrides: Overrides::default(),
        }
    }
}

impl Theme {
    /// The theme as the host stores it: opaque JSON the kernel never reads.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// Read a theme back from a stored snapshot.
    pub fn from_snapshot(value: &serde_json::Value) -> Result<Theme, String> {
        serde_json::from_value(value.clone())
            .map_err(|e| format!("cannot read the saved theme ({e})"))
    }
}

/// One voice's resolved colours.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct VoiceStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
}

/// The resolved colours every voice paints with.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Palette {
    pub user: VoiceStyle,
    pub assistant: VoiceStyle,
    pub tool: VoiceStyle,
    pub system: VoiceStyle,
}

impl Palette {
    pub fn style(&self, voice: Voice) -> VoiceStyle {
        match voice {
            Voice::User => self.user,
            Voice::Assistant => self.assistant,
            Voice::Tool => self.tool,
            Voice::System => self.system,
        }
    }

    fn set(&mut self, voice: Voice, style: VoiceStyle) {
        match voice {
            Voice::User => self.user = style,
            Voice::Assistant => self.assistant = style,
            Voice::Tool => self.tool = style,
            Voice::System => self.system = style,
        }
    }

    /// Resolve a theme into the colours to paint with.
    ///
    /// A broken theme never takes the transcript down with it: the pack
    /// falls back to the default one and the offending entries are reported
    /// so the user can fix them. Silently painting something else would be a
    /// lie about what the user asked for.
    pub fn resolve(theme: &Theme) -> (Palette, Vec<String>) {
        let mut problems = Vec::new();
        let pack = if is_known_pack(&theme.pack) {
            theme.pack.as_str()
        } else {
            problems.push(format!(
                "unknown theme pack '{}'; using '{DEFAULT_PACK}'",
                theme.pack
            ));
            DEFAULT_PACK
        };
        let mut palette = Palette::default();
        for voice in Voice::ALL {
            palette.set(voice, pack_style(pack, voice));
        }
        for voice in Voice::ALL {
            let slots = theme.overrides.slots(voice);
            for slot in [Slot::Foreground, Slot::Background] {
                let Some(raw) = slots.get(slot) else {
                    continue;
                };
                match parse_color(raw) {
                    Ok(color) => {
                        let style = palette.style(voice);
                        let mut next = style;
                        match slot {
                            Slot::Foreground => next.fg = color,
                            Slot::Background => next.bg = color,
                        }
                        palette.set(voice, next);
                    }
                    Err(e) => problems.push(format!("{}.{}: {e}", voice.name(), slot.name())),
                }
            }
        }
        (palette, problems)
    }
}

/// The names of the packs this build ships.
pub fn pack_names() -> Vec<&'static str> {
    PACKS.iter().map(|(name, _)| *name).collect()
}

/// Whether a pack name is one this build ships.
pub fn is_known_pack(name: &str) -> bool {
    PACKS.iter().any(|(pack, _)| *pack == name)
}

/// A pack's colours for one voice.
pub fn pack_style(pack: &str, voice: Voice) -> VoiceStyle {
    match (pack, voice) {
        ("default", Voice::User) => VoiceStyle {
            fg: Some(Color::Cyan),
            bg: None,
        },
        ("default", Voice::Assistant) => VoiceStyle { fg: None, bg: None },
        ("default", Voice::Tool) => VoiceStyle {
            fg: Some(Color::Gray),
            bg: None,
        },
        ("default", Voice::System) => VoiceStyle {
            fg: Some(Color::DarkGray),
            bg: None,
        },
        ("high-contrast", Voice::User) => VoiceStyle {
            fg: Some(Color::LightCyan),
            bg: None,
        },
        ("high-contrast", Voice::Assistant) => VoiceStyle {
            fg: Some(Color::White),
            bg: None,
        },
        ("high-contrast", Voice::Tool) => VoiceStyle {
            fg: Some(Color::LightYellow),
            bg: None,
        },
        ("high-contrast", Voice::System) => VoiceStyle {
            fg: Some(Color::Gray),
            bg: None,
        },
        _ => VoiceStyle::default(),
    }
}

/// The colour a user asked for.
///
/// `none` (and `default`) means "no colour of my own". An unknown name is an
/// error, never a guess: painting a colour the user did not ask for is worse
/// than telling them the name is wrong.
pub fn parse_color(raw: &str) -> Result<Option<Color>, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("none") || raw.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    if let Some(hex) = raw.strip_prefix('#') {
        if hex.len() == 6 {
            if let Ok(v) = u32::from_str_radix(hex, 16) {
                return Ok(Some(Color::Rgb(
                    (v >> 16) as u8,
                    ((v >> 8) & 0xff) as u8,
                    (v & 0xff) as u8,
                )));
            }
        }
        return Err(format!("'{raw}' is not a #rrggbb colour"));
    }
    let color = match raw.to_ascii_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        "lightred" => Some(Color::LightRed),
        "lightgreen" => Some(Color::LightGreen),
        "lightyellow" => Some(Color::LightYellow),
        "lightblue" => Some(Color::LightBlue),
        "lightmagenta" => Some(Color::LightMagenta),
        "lightcyan" => Some(Color::LightCyan),
        "white" => Some(Color::White),
        "mustard" => Some(Color::Rgb(0xc8, 0xa0, 0x28)),
        _ => None,
    };
    match color {
        Some(c) => Ok(Some(c)),
        None => Err(format!(
            "unknown colour '{raw}'; try a name, #rrggbb, or none"
        )),
    }
}

/// How a colour is written back to the user.
pub fn color_name(color: Option<Color>) -> String {
    match color {
        None => "none".to_string(),
        Some(Color::Rgb(r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
        Some(other) => format!("{other:?}").to_lowercase(),
    }
}

/// The value that clears an override and falls back to the pack.
pub const RESET: &str = "reset";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_pack_matches_the_colours_turya_has_always_used() {
        let (palette, problems) = Palette::resolve(&Theme::default());
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(palette.user.fg, Some(Color::Cyan));
        assert_eq!(palette.assistant.fg, None);
        assert_eq!(palette.tool.fg, Some(Color::Gray));
        assert_eq!(palette.system.fg, Some(Color::DarkGray));
        for voice in Voice::ALL {
            assert_eq!(palette.style(voice).bg, None, "no background by default");
        }
    }

    #[test]
    fn mustard_is_a_colour_the_user_can_ask_for() {
        assert_eq!(
            parse_color("mustard"),
            Ok(Some(Color::Rgb(0xc8, 0xa0, 0x28)))
        );
        assert_eq!(
            parse_color("MUSTARD"),
            Ok(Some(Color::Rgb(0xc8, 0xa0, 0x28)))
        );
    }

    #[test]
    fn an_unknown_colour_is_reported_rather_than_guessed() {
        assert!(parse_color("chartreuse").is_err());
        assert!(parse_color("#12345").is_err());
        assert_eq!(parse_color("none"), Ok(None));
        assert_eq!(parse_color("default"), Ok(None));
        assert_eq!(
            parse_color("#112233"),
            Ok(Some(Color::Rgb(0x11, 0x22, 0x33)))
        );
    }

    #[test]
    fn an_override_replaces_one_slot_and_leaves_the_rest_alone() {
        let mut theme = Theme::default();
        theme.overrides.tool.fg = Some("mustard".to_string());
        let (palette, problems) = Palette::resolve(&theme);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(palette.tool.fg, Some(Color::Rgb(0xc8, 0xa0, 0x28)));
        assert_eq!(palette.user.fg, Some(Color::Cyan));
        assert_eq!(palette.system.fg, Some(Color::DarkGray));
    }

    #[test]
    fn a_broken_override_keeps_the_pack_colour_and_says_so() {
        let mut theme = Theme::default();
        theme.overrides.tool.fg = Some("chartreuse".to_string());
        let (palette, problems) = Palette::resolve(&theme);
        assert_eq!(palette.tool.fg, Some(Color::Gray), "the pack colour stands");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("tool.fg"), "{problems:?}");
    }

    #[test]
    fn an_unknown_pack_falls_back_to_the_default_and_says_so() {
        let theme = Theme {
            pack: "solarized".to_string(),
            overrides: Overrides::default(),
        };
        let (palette, problems) = Palette::resolve(&theme);
        assert_eq!(palette.user.fg, Some(Color::Cyan));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("solarized"), "{problems:?}");
    }

    #[test]
    fn a_pack_can_be_switched_without_losing_the_overrides() {
        let mut theme = Theme::default();
        theme.overrides.tool.fg = Some("mustard".to_string());
        theme.pack = "high-contrast".to_string();
        let (palette, problems) = Palette::resolve(&theme);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(palette.user.fg, Some(Color::LightCyan));
        assert_eq!(
            palette.tool.fg,
            Some(Color::Rgb(0xc8, 0xa0, 0x28)),
            "the override still wins"
        );
    }

    #[test]
    fn a_snapshot_round_trips_through_opaque_json() {
        let mut theme = Theme::default();
        theme.overrides.tool.fg = Some("mustard".to_string());
        theme.overrides.user.bg = Some("#101820".to_string());
        let snapshot = theme.snapshot();
        assert!(snapshot.get("overrides").is_some());
        assert!(snapshot["overrides"]["tool"]["fg"] == serde_json::json!("mustard"));
        let back = Theme::from_snapshot(&snapshot).expect("round trip");
        assert_eq!(back, theme);
    }

    #[test]
    fn a_snapshot_without_overrides_stays_small() {
        let snapshot = Theme::default().snapshot();
        assert_eq!(snapshot["pack"], serde_json::json!("default"));
        assert!(snapshot.get("overrides").is_none());
    }

    #[test]
    fn a_snapshot_that_is_not_a_theme_is_refused() {
        let junk = serde_json::json!({"pack": 7});
        assert!(Theme::from_snapshot(&junk).is_err());
    }

    #[test]
    fn slot_keys_name_a_voice_and_a_slot() {
        assert_eq!(
            parse_slot_key("tool.fg"),
            Some((Voice::Tool, Slot::Foreground))
        );
        assert_eq!(
            parse_slot_key("user.bg"),
            Some((Voice::User, Slot::Background))
        );
        assert_eq!(parse_slot_key("tool"), None);
        assert_eq!(parse_slot_key("tool.fancy"), None);
        assert_eq!(parse_slot_key("nope.fg"), None);
    }
}
