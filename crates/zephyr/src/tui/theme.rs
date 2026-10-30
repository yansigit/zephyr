//! The Zephyr palette, installed into rpi-tui's process-wide theme. One theme, two variants:
//! dark and light, picked from the terminal's background.

use rpi_tui::theme::{Color, Theme, ThemeColors};
use std::sync::atomic::{AtomicBool, Ordering};

/// The tokens Zephyr's own components read. rpi-tui's components read the same values from
/// `rpi_tui::theme()`, set by [`install`].
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub accent: Color,
    pub text: Color,
    pub muted: Color,
    /// Rules and separators only: below text contrast by design.
    pub dim: Color,
    pub surface: Color,
    /// The band behind your messages: plainly off the background, text still 4.5:1 on it.
    pub user_bg: Color,
    pub border: Color,
    pub border_muted: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub thinking: Color,
    pub md_heading: Color,
    pub md_code: Color,
    pub md_link: Color,
    /// The header's cloud outline: a graphic, so 3:1 against the background.
    pub cloud: Color,
    /// The cloud asleep: dimmer than `cloud`, still 3:1 (`dim` falls just short on white).
    pub cloud_asleep: Color,
    /// The cloud's cheeks, beside its face: 3:1 too, so they do not fade into a light screen.
    pub cloud_blush: Color,
    /// The bar and label of a message per channel; the terminal and `chat` use `accent`.
    pub discord: Color,
    pub imessage: Color,
    /// Jobs and any other channel.
    pub other: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// On a background near #0f1419.
pub const DARK: Palette = Palette {
    accent: rgb(0x5EC8E5),
    text: rgb(0xD8DEE9),
    muted: rgb(0x8A94A6),
    dim: rgb(0x5C6675),
    surface: rgb(0x1A2230),
    user_bg: rgb(0x283040),
    border: rgb(0x2C3848),
    border_muted: rgb(0x222B38),
    success: rgb(0x7FD1A8),
    warning: rgb(0xE6B866),
    error: rgb(0xF07B7B),
    thinking: rgb(0x7F8AA0),
    md_heading: rgb(0x9AD8F0),
    md_code: rgb(0xF0C987),
    md_link: rgb(0x5EC8E5),
    cloud: rgb(0xDCEAF2),
    cloud_asleep: rgb(0x5C6675),
    cloud_blush: rgb(0xF28AA5),
    discord: rgb(0xB4A6FF),
    imessage: rgb(0x6CB6FF),
    other: rgb(0xD9B38C),
};

/// On a background near #ffffff.
pub const LIGHT: Palette = Palette {
    accent: rgb(0x0B6F91),
    text: rgb(0x1F2933),
    muted: rgb(0x56606E),
    dim: rgb(0x8C96A3),
    surface: rgb(0xEDF3F7),
    user_bg: rgb(0xE2E6EB),
    border: rgb(0xC9D3DD),
    border_muted: rgb(0xDDE4EA),
    success: rgb(0x22774B),
    warning: rgb(0x8A5D00),
    error: rgb(0xB3261E),
    thinking: rgb(0x6B7480),
    md_heading: rgb(0x0B6F91),
    md_code: rgb(0x8A4B00),
    md_link: rgb(0x0B6F91),
    cloud: rgb(0x4F93B5),
    cloud_asleep: rgb(0x808A97),
    cloud_blush: rgb(0xE0447A),
    discord: rgb(0x5B45C9),
    imessage: rgb(0x0B62B8),
    other: rgb(0x7A5230),
};

/// `NO_COLOR`: every token is the terminal's default; bold and italic still mark structure.
pub const PLAIN: Palette = Palette {
    accent: Color::Default,
    text: Color::Default,
    muted: Color::Default,
    dim: Color::Default,
    surface: Color::Default,
    user_bg: Color::Default,
    border: Color::Default,
    border_muted: Color::Default,
    success: Color::Default,
    warning: Color::Default,
    error: Color::Default,
    thinking: Color::Default,
    md_heading: Color::Default,
    md_code: Color::Default,
    md_link: Color::Default,
    cloud: Color::Default,
    cloud_asleep: Color::Default,
    cloud_blush: Color::Default,
    discord: Color::Default,
    imessage: Color::Default,
    other: Color::Default,
};

impl Palette {
    /// The colour that marks `channel`'s messages.
    pub fn channel(&self, channel: &str) -> Color {
        match channel {
            "discord" => self.discord,
            "imessage" => self.imessage,
            "tui" | "chat" => self.accent,
            _ => self.other,
        }
    }
}

static UNICODE: AtomicBool = AtomicBool::new(true);

/// Picks the palette and glyph set from the environment and the terminal, then installs it.
/// `background` is the terminal's answer to an OSC 11 query, when it gave one.
pub fn install(background: Option<rpi_tui::RgbColor>) -> Palette {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    UNICODE.store(
        utf8_locale(
            env("LC_ALL")
                .or_else(|| env("LC_CTYPE"))
                .or_else(|| env("LANG"))
                .as_deref(),
        ),
        Ordering::Relaxed,
    );
    let palette = if env("NO_COLOR").is_some() {
        PLAIN
    } else if is_light(background, env("COLORFGBG").as_deref()) {
        LIGHT
    } else {
        DARK
    };
    rpi_tui::global_theme_manager().set(theme(&palette));
    palette
}

/// The OSC 11 answer wins; else `COLORFGBG` (`fg;bg`, bg 7 or 9-15 is light); else dark.
pub fn is_light(background: Option<rpi_tui::RgbColor>, colorfgbg: Option<&str>) -> bool {
    if let Some(bg) = background {
        return bg.is_light();
    }
    colorfgbg
        .and_then(|v| v.rsplit(';').next())
        .and_then(|bg| bg.parse::<u8>().ok())
        .is_some_and(|bg| bg == 7 || (9..=15).contains(&bg))
}

fn utf8_locale(locale: Option<&str>) -> bool {
    locale.is_some_and(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("utf-8") || l.contains("utf8")
    })
}

/// Whether the locale is UTF-8, so block and box glyphs can be drawn.
pub fn unicode() -> bool {
    UNICODE.load(Ordering::Relaxed)
}

/// A glyph, or its ASCII stand-in when the locale is not UTF-8.
pub fn glyph(unicode: &'static str) -> &'static str {
    if UNICODE.load(Ordering::Relaxed) {
        return unicode;
    }
    match unicode {
        "≋" | "∼" | "≈" => "~",
        "▸" => ">",
        "▾" => "v",
        "✓" => "+",
        "✗" => "x",
        "●" => "*",
        "○" => "o",
        "◐" => "o",
        "⏸" => "||",
        "↑" => "^",
        "↑↓" => "up/down",
        "·" => "-",
        "─" => "-",
        "╰" => "+",
        "│" | "├" => "|",
        "…" => "...",
        "⏎" => "enter",
        "⇧" => "shift+",
        "›" | "▌" => ">",
        "“" | "”" => "\"",
        _ => unicode,
    }
}

fn theme(p: &Palette) -> Theme {
    let colors = ThemeColors {
        text: p.text,
        muted: p.muted,
        dim: p.dim,
        accent: p.accent,
        error: p.error,
        success: p.success,
        warning: p.warning,
        info: p.accent,
        background: Color::Default,
        surface: p.surface,
        border: p.border,
        border_accent: p.accent,
        border_muted: p.border_muted,
        selection: p.border,
        cursor: p.accent,
        thinking_text: p.thinking,
        md_heading: p.md_heading,
        md_link: p.md_link,
        md_link_url: p.muted,
        md_code: p.md_code,
        md_code_bg: p.surface,
        md_code_block: p.text,
        md_code_block_bg: p.surface,
        md_code_block_border: p.border,
        md_quote: p.muted,
        md_quote_border: p.border,
        md_hr: p.border,
        md_list_bullet: p.accent,
        tool_pending_bg: p.surface,
        tool_success_bg: p.surface,
        tool_error_bg: p.surface,
        tool_title: p.text,
        tool_output: p.muted,
        bash_mode: p.accent,
        custom_message_bg: p.surface,
        custom_message_label: p.accent,
        custom_message_text: p.text,
        tool_diff_added: p.success,
        tool_diff_removed: p.error,
        tool_diff_context: p.muted,
    };
    Theme {
        colors,
        ..Theme::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2 relative luminance (linearized sRGB). rpi-tui's `RgbColor::luminance` skips the
    /// linearization, so it cannot give a contrast ratio.
    fn luminance(c: Color) -> f64 {
        let Color::Rgb(r, g, b) = c else {
            panic!("not an rgb token: {c:?}")
        };
        let lin = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn text_tokens_reach_4_5_to_1_on_their_background() {
        for (name, p, bg) in [
            ("dark", DARK, rgb(0x0f1419)),
            ("light", LIGHT, rgb(0xffffff)),
        ] {
            for (token, c) in [
                ("accent", p.accent),
                ("text", p.text),
                ("muted", p.muted),
                ("success", p.success),
                ("warning", p.warning),
                ("error", p.error),
                ("thinking", p.thinking),
                ("md_heading", p.md_heading),
                ("md_code", p.md_code),
                ("md_link", p.md_link),
                ("discord", p.discord),
                ("imessage", p.imessage),
                ("other", p.other),
            ] {
                let ratio = contrast(c, bg);
                assert!(ratio >= 4.5, "{name} {token}: {ratio:.2}:1");
            }
            // Your messages sit on the user band: their text and channel labels.
            for (token, c) in [
                ("text", p.text),
                ("accent", p.accent),
                ("discord", p.discord),
                ("imessage", p.imessage),
                ("other", p.other),
            ] {
                let ratio = contrast(c, p.user_bg);
                assert!(ratio >= 4.5, "{name} {token} on user_bg: {ratio:.2}:1");
            }
            // The band itself is plainly visible, unlike the surface tint.
            let ratio = contrast(p.user_bg, bg);
            assert!(
                ratio >= 1.2,
                "{name} user_bg on the background: {ratio:.2}:1"
            );
            // The mascot is line art on the background, its face in `text`: 3:1 for every
            // part, asleep and cheeks included.
            for (what, c) in [
                ("cloud", p.cloud),
                ("cloud_asleep", p.cloud_asleep),
                ("cloud_blush", p.cloud_blush),
            ] {
                let ratio = contrast(c, bg);
                assert!(ratio >= 3.0, "{name} {what}: {ratio:.2}:1");
            }
        }
    }

    #[test]
    fn light_comes_from_the_terminal_then_colorfgbg() {
        let white = rpi_tui::RgbColor::new(255, 255, 255);
        let black = rpi_tui::RgbColor::new(0, 0, 0);
        assert!(is_light(Some(white), Some("15;0")));
        assert!(!is_light(Some(black), Some("0;15")));
        assert!(is_light(None, Some("0;15")));
        assert!(is_light(None, Some("0;default;7")));
        assert!(!is_light(None, Some("15;0")));
        assert!(!is_light(None, Some("garbage")));
        assert!(!is_light(None, None));
    }

    #[test]
    fn utf8_is_read_from_the_locale() {
        assert!(utf8_locale(Some("en_US.UTF-8")));
        assert!(utf8_locale(Some("C.utf8")));
        assert!(!utf8_locale(Some("C")));
        assert!(!utf8_locale(None));
    }
}
