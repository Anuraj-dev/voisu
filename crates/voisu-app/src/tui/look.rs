//! How the interface looks on this terminal: colour or none, Unicode or ASCII.
//!
//! Named ANSI colours only, so the terminal theme carries through. Colour is
//! never the only signal: every state also has a word and a glyph, and the
//! selected row has its `▌` marker in every mode.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Look {
    pub color: bool,
    pub ascii: bool,
}

/// What a piece of text means. Colour roles follow `DESIGN.md`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tone {
    Plain,
    Dim,
    Bold,
    /// What to look at first: the name, the open tab, keys, the highlight.
    /// Bold everywhere; cyan too where there is colour.
    Accent,
    Recording,
    Processing,
    Success,
    Caution,
    Failure,
}

impl Look {
    pub fn from_env() -> Self {
        let var = |name| std::env::var(name).ok();
        Self::resolve(
            var("NO_COLOR").as_deref(),
            var("TERM").as_deref(),
            [
                var("LC_ALL").as_deref(),
                var("LC_CTYPE").as_deref(),
                var("LANG").as_deref(),
            ],
        )
    }

    /// `locale` is `LC_ALL`, `LC_CTYPE` and `LANG` in POSIX precedence order.
    fn resolve(no_color: Option<&str>, term: Option<&str>, locale: [Option<&str>; 3]) -> Self {
        let effective = locale.into_iter().flatten().find(|value| !value.is_empty());
        let utf8 = effective.is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("utf-8") || value.contains("utf8")
        });
        Self {
            color: no_color.is_none_or(str::is_empty),
            ascii: term == Some("linux") || !utf8,
        }
    }

    /// The text this terminal can show. Every glyph the interface uses has an
    /// ASCII stand-in; anything else non-ASCII becomes `?`.
    pub fn text(&self, text: &str) -> String {
        if !self.ascii {
            return text.to_owned();
        }
        let mut out = String::with_capacity(text.len());
        let replaced = text.replace("←→", "<->").replace("↑↓", "up/dn");
        for ch in replaced.chars() {
            match ch {
                '▌' => out.push('>'),
                '●' => out.push('*'),
                '✕' => out.push('x'),
                '✓' => out.push('+'),
                '━' => out.push('='),
                '─' => out.push('-'),
                '│' => out.push('|'),
                '↵' => out.push_str("Enter"),
                '←' => out.push_str("Left"),
                '→' => out.push_str("Right"),
                '↑' => out.push_str("Up"),
                '↓' => out.push_str("Down"),
                '…' => out.push_str("..."),
                '·' | '—' | '–' => out.push('-'),
                '•' => out.push('*'),
                '“' | '”' => out.push('"'),
                '‘' | '’' => out.push('\''),
                _ if ch.is_ascii() => out.push(ch),
                _ => out.push('?'),
            }
        }
        out
    }

    pub fn style(&self, tone: Tone) -> Style {
        let base = Style::default();
        let colored = |color: Color| {
            if self.color { base.fg(color) } else { base }
        };
        match tone {
            Tone::Plain => base,
            Tone::Dim => base.add_modifier(Modifier::DIM),
            Tone::Bold => base.add_modifier(Modifier::BOLD),
            Tone::Accent => colored(Color::Cyan).add_modifier(Modifier::BOLD),
            Tone::Recording | Tone::Success => colored(Color::Green),
            Tone::Processing => colored(Color::Blue),
            Tone::Caution => colored(Color::Yellow),
            Tone::Failure => colored(Color::Red),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTF8: [Option<&str>; 3] = [None, None, Some("en_US.UTF-8")];

    #[test]
    fn no_color_turns_colour_off_and_empty_does_not() {
        assert!(!Look::resolve(Some("1"), Some("xterm"), UTF8).color);
        assert!(Look::resolve(Some(""), Some("xterm"), UTF8).color);
        assert!(Look::resolve(None, Some("xterm"), UTF8).color);
    }

    #[test]
    fn ascii_follows_the_linux_console_and_the_locale() {
        assert!(!Look::resolve(None, Some("xterm"), UTF8).ascii);
        assert!(Look::resolve(None, Some("linux"), UTF8).ascii);
        assert!(Look::resolve(None, Some("xterm"), [None, None, Some("C")]).ascii);
        assert!(Look::resolve(None, Some("xterm"), [None, None, None]).ascii);
        // LC_ALL wins over LANG; an empty value does not count as set.
        assert!(Look::resolve(None, Some("xterm"), [Some("C"), None, Some("en.UTF-8")]).ascii);
        assert!(!Look::resolve(None, Some("xterm"), [Some(""), None, Some("en.utf8")]).ascii);
    }

    #[test]
    fn ascii_text_never_leaves_a_non_ascii_character() {
        let look = Look {
            color: false,
            ascii: true,
        };
        let text = look.text("▌ ● ✕ ✓ ━ │ ↵ ←→ ↑↓ … · — • 日本");
        assert!(text.is_ascii(), "{text}");
        assert_eq!(look.text("←→ tabs"), "<-> tabs");
    }

    #[test]
    fn no_colour_style_carries_no_colour_at_all() {
        let look = Look {
            color: false,
            ascii: false,
        };
        for tone in [
            Tone::Plain,
            Tone::Dim,
            Tone::Bold,
            Tone::Accent,
            Tone::Recording,
            Tone::Processing,
            Tone::Success,
            Tone::Caution,
            Tone::Failure,
        ] {
            assert_eq!(look.style(tone).fg, None);
            assert_eq!(look.style(tone).bg, None);
        }
    }
}
