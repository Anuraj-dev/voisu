//! Formatting command language (`D_cmd-A`, Smart Writing §4).
//!
//! Recognizes explicit `command <phrase>` sequences in a Validated Transcript,
//! records their source spans, and structurally renders them for Literal mode.
//! Bare command-like words stay ordinary speech. No network, no Smart casing
//! or sentence heuristics (those belong to SW3).

/// Half-open UTF-8 byte range `[start, end)` into the Validated Transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SourceSpan {
    pub start: usize,
    pub end: usize,
}

impl SourceSpan {
    #[must_use]
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end);
        Self { start, end }
    }

    #[must_use]
    pub fn len(self) -> usize {
        self.end - self.start
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// Closed v1 command catalog after a successful `command` match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandKind {
    Period,
    Comma,
    QuestionMark,
    ExclamationPoint,
    NewLine,
    NewParagraph,
    Quote {
        /// Trimmed interior content between open and close phrases.
        interior: SourceSpan,
        open: SourceSpan,
        close: SourceSpan,
    },
    NumberedList {
        items: Vec<NumberedListItem>,
    },
}

/// One marker in a recognized ordered-list run (`1.` / `2.` / `3.`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NumberedListItem {
    /// 1, 2, or 3.
    pub number: u8,
    /// Non-empty item text span (UTF-8 half-open into the Validated Transcript).
    pub text_span: SourceSpan,
    /// Span of the `command number <word>` marker phrase (no item text).
    pub marker_span: SourceSpan,
}

/// One segment of a parsed Validated Transcript.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandEvent {
    /// Ordinary spoken text, including residual words after a `literal` escape.
    Text {
        text: String,
        span: SourceSpan,
    },
    /// A recognized, consumed formatting command.
    Command {
        kind: CommandKind,
        /// Source span replaced by this command (may include adjacent whitespace
        /// consumed for punctuation / break spacing).
        span: SourceSpan,
        /// Structural rendering of this command alone (Literal oracle fragment).
        render: String,
    },
}

/// Result of parsing formatting commands out of a Validated Transcript.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedCommands {
    events: Vec<CommandEvent>,
    command_spans: Vec<SourceSpan>,
}

impl ParsedCommands {
    #[must_use]
    pub fn events(&self) -> &[CommandEvent] {
        &self.events
    }

    /// Spans of recognized commands only (not escapes, not ordinary text).
    #[must_use]
    pub fn command_spans(&self) -> &[SourceSpan] {
        &self.command_spans
    }

    /// Whether any §4 command span was recognized (drives grammar separability).
    #[must_use]
    pub fn has_command_span(&self) -> bool {
        !self.command_spans.is_empty()
    }

    /// Structural command rendering matching the Literal oracle for command fixtures.
    ///
    /// Applies only the closed catalog (punctuation, breaks, quotes, numbered lists).
    /// Does not sentence-case, add terminal periods, or run any Smart heuristic.
    #[must_use]
    pub fn render_commands_only(&self) -> String {
        let mut out = String::new();
        for event in &self.events {
            match event {
                CommandEvent::Text { text, .. } => out.push_str(text),
                CommandEvent::Command { render, .. } => out.push_str(render),
            }
        }
        out
    }
}

/// Parse explicit formatting commands in `validated_transcript` (§4 / `D_cmd-A`).
///
/// Matching is Unicode-whole-token, ASCII-case-insensitive for introducers and
/// phrases, left-to-right, longest phrase first. Precedence:
/// 1. `literal command <phrase>` escape (strip only `literal`, never reparse)
/// 2. paired quote / numbered-list runs
/// 3. scalar closed phrases
/// 4. unknown / incomplete / unmatched sequences preserved word-for-word
#[must_use]
pub fn parse_formatting_commands(validated_transcript: &str) -> ParsedCommands {
    let tokens = tokenize(validated_transcript);
    if tokens.is_empty() {
        if validated_transcript.is_empty() {
            return ParsedCommands {
                events: Vec::new(),
                command_spans: Vec::new(),
            };
        }
        // Whitespace-only input: identity.
        return ParsedCommands {
            events: vec![CommandEvent::Text {
                text: validated_transcript.to_owned(),
                span: SourceSpan::new(0, validated_transcript.len()),
            }],
            command_spans: Vec::new(),
        };
    }

    let mut events: Vec<CommandEvent> = Vec::new();
    let mut command_spans: Vec<SourceSpan> = Vec::new();
    let mut i = 0;
    // Start of a pending ordinary region in source bytes (token start, or 0 for
    // leading whitespace before the first token).
    let mut ordinary_start: Option<usize> = Some(0);

    while i < tokens.len() {
        // --- 1. literal escape (before any command match) ---
        if let Some(esc) = try_literal_escape(validated_transcript, &tokens, i) {
            flush_ordinary(
                validated_transcript,
                &mut events,
                &mut ordinary_start,
                esc.literal_span.start,
            );
            // Residual `command <phrase>` as ordinary words; never reparse.
            events.push(CommandEvent::Text {
                text: validated_transcript[esc.residual_span.start..esc.residual_span.end]
                    .to_owned(),
                span: esc.residual_span,
            });
            ordinary_start = Some(esc.residual_span.end);
            i = esc.next_index;
            continue;
        }

        // --- 2/3. real command constructs ---
        if let Some(m) = try_command_match(validated_transcript, &tokens, i) {
            flush_ordinary(
                validated_transcript,
                &mut events,
                &mut ordinary_start,
                m.replace_span.start,
            );
            command_spans.push(m.command_span);
            events.push(CommandEvent::Command {
                kind: m.kind,
                span: m.replace_span,
                render: m.render,
            });
            ordinary_start = Some(m.replace_span.end);
            i = m.next_index;
            continue;
        }

        // Ordinary token: keep scanning; flush boundaries handled around matches.
        i += 1;
    }

    // Trailing ordinary text through end of input (preserves trailing whitespace).
    if let Some(start) = ordinary_start {
        if start < validated_transcript.len() {
            events.push(CommandEvent::Text {
                text: validated_transcript[start..].to_owned(),
                span: SourceSpan::new(start, validated_transcript.len()),
            });
        }
    }

    // Drop empty text events that can appear when a command sits at the start.
    events.retain(|e| !matches!(e, CommandEvent::Text { text, .. } if text.is_empty()));

    ParsedCommands {
        events,
        command_spans,
    }
}

fn flush_ordinary(
    source: &str,
    events: &mut Vec<CommandEvent>,
    ordinary_start: &mut Option<usize>,
    up_to: usize,
) {
    if let Some(start) = ordinary_start.take() {
        if start < up_to {
            events.push(CommandEvent::Text {
                text: source[start..up_to].to_owned(),
                span: SourceSpan::new(start, up_to),
            });
        }
    }
}

#[derive(Clone, Debug)]
struct Token {
    /// Half-open byte span of this whole token in the source.
    span: SourceSpan,
    /// ASCII-lowercased copy for introducer/phrase matching.
    lower: String,
}

fn tokenize(source: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut char_iter = source.char_indices().peekable();
    while let Some((start, ch)) = char_iter.next() {
        if ch.is_whitespace() {
            continue;
        }
        let mut end = start + ch.len_utf8();
        while let Some(&(next_i, next_ch)) = char_iter.peek() {
            if next_ch.is_whitespace() {
                break;
            }
            end = next_i + next_ch.len_utf8();
            char_iter.next();
        }
        let raw = &source[start..end];
        tokens.push(Token {
            span: SourceSpan::new(start, end),
            lower: ascii_lowercase(raw),
        });
    }
    tokens
}

fn ascii_lowercase(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

struct EscapeMatch {
    /// Span of the `literal` token only (not residual command text).
    literal_span: SourceSpan,
    /// Span of residual `command <phrase>` (and interior for pairs/lists) in source.
    residual_span: SourceSpan,
    next_index: usize,
}

/// `literal command <construct>` → strip only `literal`, emit residual as words.
fn try_literal_escape(source: &str, tokens: &[Token], i: usize) -> Option<EscapeMatch> {
    if i + 1 >= tokens.len() {
        return None;
    }
    if tokens[i].lower != "literal" || tokens[i + 1].lower != "command" {
        return None;
    }
    // Match the same construct that would fire after `command`, starting at i+1.
    let inner = try_command_match(source, tokens, i + 1)?;
    // Residual runs from the `command` token through the end of the construct's
    // command tokens (not the whitespace-expanded replace span). Use command_span
    // which covers introducer+phrase (and pair/list full command region).
    let residual_start = tokens[i + 1].span.start;
    let residual_end = inner.command_span.end;
    // For lists/quotes, command_span already covers through last marker/item or
    // close phrase; for scalars through the phrase end.
    Some(EscapeMatch {
        literal_span: tokens[i].span,
        residual_span: SourceSpan::new(residual_start, residual_end),
        next_index: inner.next_index,
    })
}

struct CommandMatch {
    kind: CommandKind,
    /// Span of the command itself (introducer + phrase / pair / list markers+items),
    /// used for `has_command_span` / grammar separability — not expanded for spacing.
    command_span: SourceSpan,
    /// Span replaced in the source when rendering (may include adjacent whitespace).
    replace_span: SourceSpan,
    render: String,
    next_index: usize,
}

fn try_command_match(source: &str, tokens: &[Token], i: usize) -> Option<CommandMatch> {
    if i >= tokens.len() || tokens[i].lower != "command" {
        return None;
    }
    // Paired / ranged before scalar.
    if let Some(m) = try_quote(source, tokens, i) {
        return Some(m);
    }
    if let Some(m) = try_numbered_list(source, tokens, i) {
        return Some(m);
    }
    try_scalar(source, tokens, i)
}

/// Closed scalar phrases, longest first.
fn try_scalar(source: &str, tokens: &[Token], i: usize) -> Option<CommandMatch> {
    // (phrase tokens after `command`, kind, render char/str)
    // Longest multi-token phrases first.
    let multi: &[(&[&str], CommandKind, &str)] = &[
        (
            &["exclamation", "point"],
            CommandKind::ExclamationPoint,
            "!",
        ),
        (&["question", "mark"], CommandKind::QuestionMark, "?"),
        (&["new", "paragraph"], CommandKind::NewParagraph, "\n\n"),
        (&["new", "line"], CommandKind::NewLine, "\n"),
    ];
    for &(phrase, ref kind, render) in multi {
        if phrase_matches(tokens, i + 1, phrase) {
            return Some(make_scalar_match(
                source,
                tokens,
                i,
                phrase.len(),
                kind.clone(),
                render,
            ));
        }
    }
    let single: &[(&str, CommandKind, &str)] = &[
        ("period", CommandKind::Period, "."),
        ("comma", CommandKind::Comma, ","),
    ];
    for &(word, ref kind, render) in single {
        if phrase_matches(tokens, i + 1, &[word]) {
            return Some(make_scalar_match(
                source,
                tokens,
                i,
                1,
                kind.clone(),
                render,
            ));
        }
    }
    None
}

fn phrase_matches(tokens: &[Token], start: usize, phrase: &[&str]) -> bool {
    if start + phrase.len() > tokens.len() {
        return false;
    }
    phrase
        .iter()
        .enumerate()
        .all(|(k, word)| tokens[start + k].lower == *word)
}

fn make_scalar_match(
    source: &str,
    tokens: &[Token],
    command_index: usize,
    phrase_len: usize,
    kind: CommandKind,
    render: &str,
) -> CommandMatch {
    let last = command_index + phrase_len; // index of last phrase token
    let command_span = SourceSpan::new(tokens[command_index].span.start, tokens[last].span.end);
    let next_index = last + 1;
    let replace_span = expand_replace_span(source, tokens, command_index, last, &kind);
    CommandMatch {
        kind,
        command_span,
        replace_span,
        render: render.to_owned(),
        next_index,
    }
}

/// Expand the replaced region to absorb adjacent whitespace for spacing rules.
///
/// - Punctuation / new line / new paragraph: consume whitespace immediately before
///   the introducer (so `word command period` → `word.`) and, for breaks only,
///   whitespace immediately after the phrase (so the next line has no indent).
/// - Quote / list: do not consume leading whitespace (space before `"` stays).
fn expand_replace_span(
    source: &str,
    tokens: &[Token],
    command_index: usize,
    last_phrase_index: usize,
    kind: &CommandKind,
) -> SourceSpan {
    let mut start = tokens[command_index].span.start;
    let mut end = tokens[last_phrase_index].span.end;

    let consumes_leading_ws = matches!(
        kind,
        CommandKind::Period
            | CommandKind::Comma
            | CommandKind::QuestionMark
            | CommandKind::ExclamationPoint
            | CommandKind::NewLine
            | CommandKind::NewParagraph
            | CommandKind::NumberedList { .. }
    );
    if consumes_leading_ws {
        // Include whitespace between previous token (if any) and this command.
        let ws_start = if command_index > 0 {
            tokens[command_index - 1].span.end
        } else {
            // Leading command: only consume whitespace that immediately precedes it.
            // Keep any content-free prefix only if it is whitespace.
            let prefix = &source[..start];
            if prefix.chars().all(|c| c.is_whitespace()) {
                0
            } else {
                start
            }
        };
        // Walk back over whitespace only.
        let before = &source[ws_start..start];
        if !before.is_empty() && before.chars().all(|c| c.is_whitespace()) {
            start = ws_start;
        } else if command_index > 0 {
            // Only the inter-token whitespace.
            start = tokens[command_index - 1].span.end;
            // But only if that region is whitespace (it should be).
            if !source[start..tokens[command_index].span.start]
                .chars()
                .all(|c| c.is_whitespace())
            {
                start = tokens[command_index].span.start;
            }
        }
    }

    let consumes_trailing_ws = matches!(
        kind,
        CommandKind::NewLine | CommandKind::NewParagraph | CommandKind::NumberedList { .. }
    );
    if consumes_trailing_ws {
        let ws_end = if last_phrase_index + 1 < tokens.len() {
            tokens[last_phrase_index + 1].span.start
        } else {
            // Trailing: consume trailing whitespace to EOF.
            source.len()
        };
        if ws_end > end && source[end..ws_end].chars().all(|c| c.is_whitespace()) {
            end = ws_end;
        }
    }

    SourceSpan::new(start, end)
}

fn try_quote(source: &str, tokens: &[Token], i: usize) -> Option<CommandMatch> {
    // command quote … command unquote
    if !phrase_matches(tokens, i + 1, &["quote"]) {
        return None;
    }
    let open_last = i + 1; // "quote"
    let open_span = SourceSpan::new(tokens[i].span.start, tokens[open_last].span.end);

    // Find matching `command unquote` (no nested command parsing inside).
    let mut j = open_last + 1;
    while j + 1 < tokens.len() {
        if tokens[j].lower == "command" && tokens[j + 1].lower == "unquote" {
            let close_span = SourceSpan::new(tokens[j].span.start, tokens[j + 1].span.end);
            let interior_raw_start = tokens[open_last].span.end;
            let interior_raw_end = tokens[j].span.start;
            let interior = trim_span(source, interior_raw_start, interior_raw_end);
            // Empty interior is still a valid pair ("").
            let command_span = SourceSpan::new(open_span.start, close_span.end);
            // Do not consume leading whitespace before `command quote`.
            // Do not consume trailing whitespace after unquote (space before next word stays).
            let replace_span = command_span;
            let interior_text = &source[interior.start..interior.end];
            let render = format!("\"{interior_text}\"");
            return Some(CommandMatch {
                kind: CommandKind::Quote {
                    interior,
                    open: open_span,
                    close: close_span,
                },
                command_span,
                replace_span,
                render,
                next_index: j + 2,
            });
        }
        j += 1;
    }
    // Unmatched quote → ordinary speech.
    None
}

fn trim_span(source: &str, start: usize, end: usize) -> SourceSpan {
    let slice = &source[start..end];
    let leading = slice
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(slice.len());
    let trailing = slice
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    if leading >= trailing {
        SourceSpan::new(start, start)
    } else {
        SourceSpan::new(start + leading, start + trailing)
    }
}

const NUMBER_WORDS: &[&str] = &["one", "two", "three"];

fn try_numbered_list(source: &str, tokens: &[Token], i: usize) -> Option<CommandMatch> {
    // Must begin at `command number one`.
    if !phrase_matches(tokens, i + 1, &["number", "one"]) {
        return None;
    }

    let mut items: Vec<NumberedListItem> = Vec::new();
    let mut pos = i;
    let mut expected: u8 = 1;

    while expected <= 3 {
        let word = NUMBER_WORDS[(expected - 1) as usize];
        if pos >= tokens.len()
            || tokens[pos].lower != "command"
            || !phrase_matches(tokens, pos + 1, &["number", word])
        {
            break;
        }
        let marker_last = pos + 2; // "number" + word
        let marker_span = SourceSpan::new(tokens[pos].span.start, tokens[marker_last].span.end);
        let item_token_start = marker_last + 1;

        // Item text: tokens until the next `command` token (or EOF).
        let mut item_token_end = item_token_start;
        while item_token_end < tokens.len() && tokens[item_token_end].lower != "command" {
            item_token_end += 1;
        }
        if item_token_start >= item_token_end {
            // Empty item text → whole run is ordinary speech.
            return None;
        }
        let text_span = SourceSpan::new(
            tokens[item_token_start].span.start,
            tokens[item_token_end - 1].span.end,
        );
        items.push(NumberedListItem {
            number: expected,
            text_span,
            marker_span,
        });

        pos = item_token_end;
        expected += 1;

        // Continue only if the next command is the consecutive number marker.
        if expected <= 3 {
            let next_word = NUMBER_WORDS[(expected - 1) as usize];
            if pos < tokens.len()
                && tokens[pos].lower == "command"
                && phrase_matches(tokens, pos + 1, &["number", next_word])
            {
                continue;
            }
            break;
        }
    }

    if items.len() < 2 {
        return None;
    }

    let first_cmd = i;
    // Last token consumed is the last item's last text token.
    let last_item = items.last().unwrap();
    // Find token index of last item text end.
    let mut last_token_idx = first_cmd;
    for (idx, t) in tokens.iter().enumerate().skip(first_cmd) {
        if t.span.end == last_item.text_span.end {
            last_token_idx = idx;
            break;
        }
    }
    let command_span = SourceSpan::new(tokens[first_cmd].span.start, last_item.text_span.end);
    let kind = CommandKind::NumberedList {
        items: items.clone(),
    };
    // Consume leading whitespace before the list and trailing whitespace after last item token.
    let replace_span = expand_replace_span(source, tokens, first_cmd, last_token_idx, &kind);

    let mut render = String::new();
    for (n, item) in items.iter().enumerate() {
        if n > 0 {
            render.push('\n');
        }
        let text = &source[item.text_span.start..item.text_span.end];
        render.push_str(&format!("{}. {text}", item.number));
    }

    let next_index = last_token_idx + 1;
    Some(CommandMatch {
        kind: CommandKind::NumberedList { items },
        command_span,
        replace_span,
        render,
        next_index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(input: &str) -> String {
        parse_formatting_commands(input).render_commands_only()
    }

    fn has_cmd(input: &str) -> bool {
        parse_formatting_commands(input).has_command_span()
    }

    // --- Identity / bare phrases never fire ---

    #[test]
    fn bare_phrases_are_identity() {
        let cases = [
            "ship it exclamation point",
            "stop period new line next item",
            "the period of the moon is twenty seven days",
            "are you free question mark",
            "first thought new line second thought",
            "please press new line after the title when you typeset it",
            "number one apples number two oranges number three pears",
            "use the exact error quote connection refused unquote in the ticket",
            "there is two issues new line next topic",
            "yes comma I can join",
            "the bullet train is fast",
            "the quote was approved",
            "we launched a brand new line yesterday",
            "the number one rule is safety",
            "the reporting period ended yesterday",
            "intro new paragraph body text",
            "she said quote we ship friday unquote and hung up",
        ];
        for input in cases {
            assert_eq!(lit(input), input, "bare must stay identity: {input:?}");
            assert!(!has_cmd(input), "bare must not set command span: {input:?}");
        }
    }

    // --- Positive scalar / break commands ---

    #[test]
    fn f09b_exclamation() {
        assert_eq!(lit("ship it command exclamation point"), "ship it!");
        assert!(has_cmd("ship it command exclamation point"));
    }

    #[test]
    fn f10b_period_and_newline() {
        assert_eq!(
            lit("stop command period command new line next item"),
            "stop.\nnext item"
        );
    }

    #[test]
    fn f13b_newline() {
        assert_eq!(
            lit("first thought command new line second thought"),
            "first thought\nsecond thought"
        );
    }

    #[test]
    fn f14_new_paragraph() {
        assert_eq!(
            lit("intro command new paragraph body text"),
            "intro\n\nbody text"
        );
    }

    #[test]
    fn f36b_newline_separability() {
        assert_eq!(
            lit("there is two issues command new line next topic"),
            "there is two issues\nnext topic"
        );
        assert!(has_cmd("there is two issues command new line next topic"));
    }

    // --- Escape ---

    #[test]
    fn f15b_literal_escape() {
        let input = "literal command new line then continue";
        assert_eq!(lit(input), "command new line then continue");
        assert!(
            !has_cmd(input),
            "escape must not record a command span"
        );
    }

    #[test]
    fn literal_escape_never_reparses_command_words() {
        assert_eq!(
            lit("literal command period"),
            "command period"
        );
        assert_eq!(
            lit("say literal command exclamation point please"),
            "say command exclamation point please"
        );
    }

    // --- Quote pair ---

    #[test]
    fn f22b_quote() {
        assert_eq!(
            lit("use the exact error command quote connection refused command unquote in the ticket"),
            "use the exact error \"connection refused\" in the ticket"
        );
    }

    #[test]
    fn f21b_quote_preserves_interior_case() {
        assert_eq!(
            lit("she said command quote we ship friday command unquote and hung up"),
            "she said \"we ship friday\" and hung up"
        );
    }

    #[test]
    fn unmatched_quote_is_ordinary() {
        let input = "command quote hello there";
        assert_eq!(lit(input), input);
        assert!(!has_cmd(input));
    }

    #[test]
    fn unmatched_unquote_is_ordinary() {
        let input = "hello command unquote there";
        assert_eq!(lit(input), input);
        assert!(!has_cmd(input));
    }

    // --- Numbered list ---

    #[test]
    fn f16b_numbered_list() {
        assert_eq!(
            lit("command number one apples command number two oranges command number three pears"),
            "1. apples\n2. oranges\n3. pears"
        );
        assert!(has_cmd(
            "command number one apples command number two oranges command number three pears"
        ));
    }

    #[test]
    fn list_requires_at_least_two_markers() {
        let input = "command number one apples only";
        assert_eq!(lit(input), input);
        assert!(!has_cmd(input));
    }

    #[test]
    fn list_must_start_at_one() {
        let input = "command number two apples command number three oranges";
        assert_eq!(lit(input), input);
        assert!(!has_cmd(input));
    }

    #[test]
    fn list_rejects_empty_item() {
        let input = "command number one command number two oranges";
        assert_eq!(lit(input), input);
        assert!(!has_cmd(input));
    }

    #[test]
    fn list_two_items_ok() {
        assert_eq!(
            lit("command number one apples command number two oranges"),
            "1. apples\n2. oranges"
        );
    }

    // --- Unknown / incomplete ---

    #[test]
    fn unknown_command_preserved() {
        let input = "please command bold this word";
        assert_eq!(lit(input), input);
        assert!(!has_cmd(input));
    }

    #[test]
    fn incomplete_command_preserved() {
        for input in [
            "end with command",
            "command new",
            "command exclamation",
            "command question",
            "command number",
            "command number four apples",
        ] {
            assert_eq!(lit(input), input, "{input:?}");
            assert!(!has_cmd(input), "{input:?}");
        }
    }

    // --- Case insensitivity ---

    #[test]
    fn ascii_case_insensitive_introducers() {
        assert_eq!(lit("ship it COMMAND Exclamation Point"), "ship it!");
        assert_eq!(
            lit("LITERAL COMMAND New Line then"),
            "COMMAND New Line then"
        );
    }

    // --- Multi-space phrase separation ---

    #[test]
    fn multi_whitespace_between_phrase_tokens() {
        assert_eq!(lit("ship it command  exclamation   point"), "ship it!");
        assert_eq!(
            lit("first command   new    line second"),
            "first\nsecond"
        );
    }

    // --- Source spans ---

    #[test]
    fn command_spans_cover_introducer_and_phrase() {
        let parsed = parse_formatting_commands("ship it command period");
        assert_eq!(parsed.command_spans().len(), 1);
        let span = parsed.command_spans()[0];
        let src = "ship it command period";
        assert_eq!(&src[span.start..span.end], "command period");
    }

    #[test]
    fn quote_interior_span() {
        let src = "x command quote Ab C command unquote y";
        let parsed = parse_formatting_commands(src);
        let event = parsed
            .events()
            .iter()
            .find_map(|e| match e {
                CommandEvent::Command {
                    kind: CommandKind::Quote { interior, .. },
                    ..
                } => Some(*interior),
                _ => None,
            })
            .expect("quote command");
        assert_eq!(&src[event.start..event.end], "Ab C");
    }

    // --- Comma / question mark ---

    #[test]
    fn comma_and_question_mark() {
        assert_eq!(lit("yes command comma I can"), "yes, I can");
        assert_eq!(lit("free command question mark"), "free?");
    }

    // --- Corpus-driven ---

    #[test]
    fn corpus_command_and_ambiguity_fixtures() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/research/smart-writing-behavior-corpus-2026-08-09.json"
        );
        let raw = std::fs::read_to_string(path).expect("behavior corpus readable");
        let corpus: serde_json::Value =
            serde_json::from_str(&raw).expect("behavior corpus JSON");
        let fixtures = corpus["fixtures"]
            .as_array()
            .expect("fixtures array");

        // Fixtures where Literal output is exactly structural command rendering
        // (or identity for bare/ambiguity). Every fixture's expected.literal is
        // the oracle for command application alone.
        let mut checked = 0usize;
        for fx in fixtures {
            let id = fx["id"].as_str().unwrap();
            let input = fx["input"].as_str().unwrap();
            let expected_literal = fx["expected"]["literal"].as_str().unwrap();
            let roles = fx["roles"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let category = fx["category"].as_str().unwrap_or("");

            let command_related = roles.iter().any(|r| {
                *r == "command"
                    || *r == "command_safety_counterexample"
                    || r.contains("command")
            }) || category == "punctuation_command"
                || category == "line_paragraph"
                || category == "list"
                || category == "quotation"
                || category == "command_safety"
                || input.to_ascii_lowercase().contains("command")
                || id.ends_with('b')
                    && matches!(
                        id,
                        "F09b"
                            | "F10b"
                            | "F13b"
                            | "F14b"
                            | "F15b"
                            | "F16b"
                            | "F21b"
                            | "F22b"
                            | "F36b"
                    );

            // Also always check fixtures whose input mentions command-like words
            // or whose literal differs from input (command applied).
            let literal_differs = expected_literal != input;
            if !(command_related || literal_differs) {
                // Still: for pure identity fixtures without command interest, skip.
                // But command safety counterexamples are included via roles.
                continue;
            }

            let got = lit(input);
            assert_eq!(
                got, expected_literal,
                "fixture {id}: render_commands_only must match expected.literal\n  input: {input:?}\n  got:   {got:?}\n  want:  {expected_literal:?}"
            );

            if literal_differs && !input.to_ascii_lowercase().starts_with("literal ")
                && !input.to_ascii_lowercase().contains(" literal command")
            {
                // Real command applied (not merely escape) ⇒ has_command_span.
                // Escape-only (F15b): literal differs but no command span.
                let is_escape_only = parse_formatting_commands(input)
                    .events()
                    .iter()
                    .all(|e| matches!(e, CommandEvent::Text { .. }));
                if is_escape_only {
                    assert!(
                        !has_cmd(input),
                        "fixture {id}: escape-only must not set command span"
                    );
                } else {
                    assert!(
                        has_cmd(input),
                        "fixture {id}: command application must set command span"
                    );
                }
            } else if expected_literal == input {
                assert!(
                    !has_cmd(input),
                    "fixture {id}: identity literal must not set command span"
                );
            }

            checked += 1;
        }
        assert!(
            checked >= 20,
            "expected to check many command/ambiguity fixtures, checked {checked}"
        );
    }

    #[test]
    fn empty_and_plain_prose() {
        assert_eq!(lit(""), "");
        assert!(!has_cmd(""));
        assert_eq!(lit("hello world"), "hello world");
        assert!(!has_cmd("hello world"));
    }

    #[test]
    fn precedence_literal_before_command() {
        // Without literal, fires; with literal, words.
        assert_eq!(lit("x command new line y"), "x\ny");
        assert_eq!(lit("x literal command new line y"), "x command new line y");
    }
}
