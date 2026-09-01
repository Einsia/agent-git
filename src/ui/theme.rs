//! Colors and symbols.
//!
//! One place: a style change touches only this file. Every symbol has an ASCII fallback — agit
//! runs in every kind of terminal (CI logs, tmux, Windows terminals), so Unicode cannot be
//! assumed to render.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Cyan;
pub const MUTED: Color = Color::DarkGray;
pub const OK: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const ERR: Color = Color::Red;
/// Color of user prompts in the rendered transcript.
pub const PROMPT: Color = Color::Cyan;
/// Color of tool calls.
pub const TOOL: Color = Color::Magenta;

pub fn header() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn selected() -> Style {
    Style::default()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

/// Whether to use Unicode symbols.
///
/// The test is whether the locale carries UTF-8. When in doubt, ASCII — rendering as boxes is
/// worse than rendering as ASCII.
pub fn unicode_ok() -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"].iter().any(|v| {
        std::env::var(v)
            .map(|s| {
                let s = s.to_ascii_uppercase();
                s.contains("UTF-8") || s.contains("UTF8")
            })
            .unwrap_or(false)
    })
}

pub struct Symbols {
    pub active: &'static str,
    pub idle: &'static str,
    pub check: &'static str,
    pub cross: &'static str,
    pub node: &'static str,
    pub vline: &'static str,
    pub arrow: &'static str,
    pub warn: &'static str,
}

pub fn symbols() -> Symbols {
    if unicode_ok() {
        Symbols {
            active: "●",
            idle: "·",
            check: "✓",
            cross: "✗",
            node: "◆",
            vline: "│",
            arrow: "→",
            warn: "!",
        }
    } else {
        Symbols {
            active: "*",
            idle: ".",
            check: "OK",
            cross: "X",
            node: "o",
            vline: "|",
            arrow: "->",
            warn: "!",
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_symbol_is_non_empty() {
        // An empty symbol misaligns the output.
        let s = super::symbols();
        for sym in [
            s.active, s.idle, s.check, s.cross, s.node, s.vline, s.arrow, s.warn,
        ] {
            assert!(!sym.is_empty());
        }
    }
}
