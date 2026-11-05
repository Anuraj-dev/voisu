//! Width-aware text helpers shared by the model and the renderers.
//!
//! Everything shown in the interface passes through here. Transcripts and
//! provider messages are untrusted, so control characters are stripped before
//! a cell is ever written.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Cuts `text` to `max` columns, ending in an ellipsis when anything was lost.
pub fn fit(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cell = ch.width().unwrap_or(0);
        if used + cell > max - 1 {
            break;
        }
        out.push(ch);
        used += cell;
    }
    out.push('…');
    out
}

/// Word-wraps `text` to `max` columns. Explicit line breaks are kept and a word
/// longer than the line is split, so no content is ever dropped.
pub fn wrap(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for word in paragraph.split_whitespace() {
            let cells = width(word);
            if used > 0 && used + 1 + cells > max {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            for ch in word.chars() {
                let cell = ch.width().unwrap_or(0);
                if used + cell > max {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(ch);
                used += cell;
            }
        }
        lines.push(line);
    }
    lines
}

/// Drops every control character except the line break, so a hostile Transcript
/// cannot move the cursor or change colours, while its lines stay intact.
pub fn clean_block(text: &str) -> String {
    text.chars()
        .filter_map(|ch| match ch {
            '\n' => Some('\n'),
            '\t' => Some(' '),
            _ if ch.is_control() => None,
            _ => Some(ch),
        })
        .collect()
}

/// One clean line: whitespace runs collapse and control characters vanish.
pub fn clean_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().filter(|ch| !ch.is_control()));
    }
    out
}

/// First row of a window of `height` rows that keeps row `selected` visible.
pub fn window_start(selected: usize, height: usize) -> usize {
    if height == 0 || selected < height {
        0
    } else {
        selected + 1 - height
    }
}

/// Sentence-cases a lowercase setting or doctor value for display.
pub fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_ellipsizes_only_when_needed() {
        assert_eq!(fit("short", 5), "short");
        assert_eq!(fit("longer text", 6), "longe…");
        assert_eq!(fit("anything", 0), "");
    }

    #[test]
    fn fit_counts_columns_not_characters() {
        // Each of these is two columns wide.
        assert_eq!(fit("日本語テキスト", 5), "日本…");
    }

    #[test]
    fn wrap_keeps_every_word_and_every_line_break() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("first\nsecond", 20), vec!["first", "second"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn hostile_text_loses_its_control_characters() {
        assert_eq!(clean_block("a\x1b[31mb\nc\x07"), "a[31mb\nc");
        assert_eq!(clean_line("a\x1b[2J  b\n\nc"), "a[2J b c");
    }

    #[test]
    fn window_follows_the_selection() {
        assert_eq!(window_start(0, 3), 0);
        assert_eq!(window_start(2, 3), 0);
        assert_eq!(window_start(3, 3), 1);
    }
}
