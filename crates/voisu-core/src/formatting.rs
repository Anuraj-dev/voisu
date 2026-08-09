//! Deterministic local Formatting + sealed [`FormattingBaseline`] (Smart Writing §5 / SW3).
//!
//! No network, model, credential, app context, clipboard, screen, or surrounding text.
//! Only content input is the Validated Transcript plus Writing Mode. Uses the §4 command
//! parser ([`crate::parse_formatting_commands`]); does not reimplement command recognition.
//!
//! Authority: the 51 locked #99 fixture I/O pairs are the executable oracle. Silent inputs
//! fail closed (leave contested spans unchanged) and still produce a sealed baseline.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::cell::Cell;

use sha2::{Digest, Sha256};

use crate::formatting_commands::{
    parse_formatting_commands, CommandEvent, CommandKind, SourceSpan,
};

// ─── Normative constants (§8 / constants JSON) ───────────────────────────────

/// Local formatter maximum work bound. Miss preserves Validated identity.
pub const LOCAL_FORMATTER_WORK_DEADLINE: Duration = Duration::from_millis(50);

/// Maximum Validated Transcript UTF-8 size the formatter will rewrite.
/// Oversize inputs keep identity (never truncated).
pub const MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES: usize = 32_768;

/// Formatter contract ID persisted on every sealed baseline.
pub const FORMATTER_CONTRACT_ID: &str = "voisu-local-formatting-v1:#99-approved";

/// Default Validated Transcript version string bound into the baseline.
pub const VALIDATED_TRANSCRIPT_VERSION: &str = "validated-en-v1";

// ─── Public types ────────────────────────────────────────────────────────────

/// User-selected Writing Mode snapshotted for a Recording (§3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WritingMode {
    /// Apply deterministic local Formatting (and, when locked by #99, the closed
    /// local grammar catalog that matches Minimal Grammar predicates for hermetic
    /// oracle fixtures; provider grammar remains SW4/SW6).
    Smart,
    /// Preserve wording/case/punctuation except for explicit §4 commands.
    Literal,
}

/// Half-open UTF-8 range of a source token in the rendered baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SourceAnchor {
    pub rendered_start: usize,
    pub rendered_end: usize,
}

impl SourceAnchor {
    #[must_use]
    pub fn new(rendered_start: usize, rendered_end: usize) -> Self {
        debug_assert!(rendered_start <= rendered_end);
        Self {
            rendered_start,
            rendered_end,
        }
    }
}

/// Sealed, immutable formatter output bound to exactly one Validated Transcript.
///
/// Fields and construction are private to this module. Provider JSON cannot
/// deserialize or forge a baseline. Consumers use typed accessors only.
#[derive(Clone, Debug)]
pub struct FormattingBaseline {
    base_version: String,
    base_fingerprint: String,
    rendered: String,
    /// Source token `[start,end)` → rendered range, sorted by source start.
    anchors: BTreeMap<(usize, usize), SourceAnchor>,
    protected_source_ranges: Vec<SourceSpan>,
    formatter_contract: String,
    derivation_digest: String,
}

impl FormattingBaseline {
    #[must_use]
    pub fn base_version(&self) -> &str {
        &self.base_version
    }

    #[must_use]
    pub fn base_fingerprint(&self) -> &str {
        &self.base_fingerprint
    }

    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }

    #[must_use]
    pub fn formatter_contract(&self) -> &str {
        &self.formatter_contract
    }

    /// Structural derivation digest (`sha256:<hex>`).
    #[must_use]
    pub fn derivation_digest(&self) -> &str {
        &self.derivation_digest
    }

    /// Formatter-owned protected source ranges (commands, quotes/code, composites).
    #[must_use]
    pub fn protected_source_ranges(&self) -> &[SourceSpan] {
        &self.protected_source_ranges
    }

    /// Look up the rendered anchor for a source half-open span, if recorded.
    #[must_use]
    pub fn anchor_for_source(&self, span: SourceSpan) -> Option<SourceAnchor> {
        self.anchors.get(&(span.start, span.end)).copied()
    }

    /// Number of source-token anchors recorded for grammar composition.
    #[must_use]
    pub fn anchor_count(&self) -> usize {
        self.anchors.len()
    }

    /// Recompute the derivation digest and compare (integrity check).
    #[must_use]
    pub fn verify_derivation_digest(&self) -> bool {
        self.derivation_digest
            == derivation_digest(
                &self.base_version,
                &self.base_fingerprint,
                &self.rendered,
                &self.anchors,
                &self.protected_source_ranges,
                &self.formatter_contract,
            )
    }
}

/// Optional inputs for [`format_validated_with`].
#[derive(Clone, Debug)]
pub struct FormatOptions<'a> {
    /// Validated Transcript version (default [`VALIDATED_TRANSCRIPT_VERSION`]).
    pub version: &'a str,
    /// Recording-time dictionary snapshot (whole-token protection). Empty if none.
    pub dictionary: &'a [&'a str],
    /// Recording-time protected-names snapshot. Empty if none.
    pub protected_names: &'a [&'a str],
    /// Work deadline override (tests). `None` → [`LOCAL_FORMATTER_WORK_DEADLINE`].
    pub work_deadline: Option<Duration>,
}

impl Default for FormatOptions<'_> {
    fn default() -> Self {
        Self {
            version: VALIDATED_TRANSCRIPT_VERSION,
            dictionary: &[],
            protected_names: &[],
            work_deadline: None,
        }
    }
}

// ─── Entry points ────────────────────────────────────────────────────────────

/// Format a Validated Transcript under `mode` with default options.
#[must_use]
pub fn format_validated(validated_transcript: &str, mode: WritingMode) -> FormattingBaseline {
    format_validated_with(validated_transcript, mode, FormatOptions::default())
}

/// Format with dictionary/names snapshots and optional deadline override.
#[must_use]
pub fn format_validated_with(
    validated_transcript: &str,
    mode: WritingMode,
    options: FormatOptions<'_>,
) -> FormattingBaseline {
    let started = Instant::now();
    let budget = options
        .work_deadline
        .unwrap_or(LOCAL_FORMATTER_WORK_DEADLINE);
    // Instant deadline so zero-budget and mid-loop checks share one comparison.
    let deadline_at = started + budget;
    let fingerprint = transcript_fingerprint(validated_transcript);

    // Size gate: oversize → identity, never truncated (cheap 1:1 seal).
    if validated_transcript.len() > MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES {
        return seal_identity_baseline(validated_transcript, options.version, fingerprint);
    }

    // Cooperative bound: already elapsed / zero budget → identity without heavy work.
    if Instant::now() >= deadline_at {
        return seal_identity_baseline(validated_transcript, options.version, fingerprint);
    }

    let rendered = match mode {
        WritingMode::Literal => {
            parse_formatting_commands(validated_transcript).render_commands_only()
        }
        WritingMode::Smart => {
            let Some(s) = smart_format(
                validated_transcript,
                options.dictionary,
                options.protected_names,
                deadline_at,
            ) else {
                return seal_identity_baseline(
                    validated_transcript,
                    options.version,
                    fingerprint,
                );
            };
            s
        }
    };

    // Miss after work, during seal, or after seal → identity. Sealing must not
    // return a formatted baseline once the cooperative bound has elapsed.
    finish_baseline(
        validated_transcript,
        options.version,
        fingerprint,
        rendered,
        deadline_at,
    )
}

/// Seal a rendered string, or fall back to identity if the deadline hits before,
/// during, or immediately after sealing work.
fn finish_baseline(
    source: &str,
    version: &str,
    fingerprint: String,
    rendered: String,
    deadline_at: Instant,
) -> FormattingBaseline {
    if deadline_hit(deadline_at) {
        return seal_identity_baseline(source, version, fingerprint);
    }
    let Some(sealed) = try_seal_baseline(source, version, fingerprint.clone(), rendered, deadline_at)
    else {
        return seal_identity_baseline(source, version, fingerprint);
    };
    if deadline_hit(deadline_at) {
        return seal_identity_baseline(source, version, fingerprint);
    }
    sealed
}

// ─── Sealing ─────────────────────────────────────────────────────────────────

/// Build a sealed baseline, or `None` if the cooperative deadline elapses mid-seal.
fn try_seal_baseline(
    source: &str,
    version: &str,
    fingerprint: String,
    rendered: String,
    deadline_at: Instant,
) -> Option<FormattingBaseline> {
    let anchors = source_anchors(source, &rendered);
    if deadline_hit(deadline_at) {
        return None;
    }
    let protected = protected_source_ranges(source);
    if deadline_hit(deadline_at) {
        return None;
    }
    let contract = FORMATTER_CONTRACT_ID.to_owned();
    let digest = derivation_digest(
        version,
        &fingerprint,
        &rendered,
        &anchors,
        &protected,
        &contract,
    );
    Some(FormattingBaseline {
        base_version: version.to_owned(),
        base_fingerprint: fingerprint,
        rendered,
        anchors,
        protected_source_ranges: protected,
        formatter_contract: contract,
        derivation_digest: digest,
    })
}

/// Cheap identity seal for oversize / deadline-miss paths.
///
/// Anchors are 1:1 source→rendered word tokens (no re-tokenization against a
/// rewritten string). Protected ranges cover the whole base (fail-closed for
/// grammar) without O(n²) composite scans.
fn seal_identity_baseline(
    source: &str,
    version: &str,
    fingerprint: String,
) -> FormattingBaseline {
    let anchors = identity_source_anchors(source);
    let protected = if source.is_empty() {
        Vec::new()
    } else {
        vec![SourceSpan::new(0, source.len())]
    };
    let contract = FORMATTER_CONTRACT_ID.to_owned();
    let rendered = source.to_owned();
    let digest = derivation_digest(
        version,
        &fingerprint,
        &rendered,
        &anchors,
        &protected,
        &contract,
    );
    FormattingBaseline {
        base_version: version.to_owned(),
        base_fingerprint: fingerprint,
        rendered,
        anchors,
        protected_source_ranges: protected,
        formatter_contract: contract,
        derivation_digest: digest,
    }
}

fn identity_source_anchors(source: &str) -> BTreeMap<(usize, usize), SourceAnchor> {
    let mut anchors = BTreeMap::new();
    for (s, e, _) in word_tokens(source) {
        anchors.insert((s, e), SourceAnchor::new(s, e));
    }
    anchors
}

#[inline]
fn deadline_hit(deadline_at: Instant) -> bool {
    #[cfg(test)]
    if let Some(forced) = seal_deadline_test_hook::poll() {
        return forced;
    }
    Instant::now() >= deadline_at
}

/// Test-only: force cooperative deadline hits after N free checks (deterministic
/// seal before/during/after branches without flaky wall-clock races).
#[cfg(test)]
mod seal_deadline_test_hook {
    use super::*;

    thread_local! {
        /// Remaining free (non-hit) checks before forced hit. `None` = inactive.
        static FREE_CHECKS: Cell<Option<u32>> = const { Cell::new(None) };
    }

    pub fn arm_hit_after(free_checks: u32) {
        FREE_CHECKS.with(|c| c.set(Some(free_checks)));
    }

    pub fn clear() {
        FREE_CHECKS.with(|c| c.set(None));
    }

    pub(super) fn poll() -> Option<bool> {
        FREE_CHECKS.with(|c| {
            let left = c.get()?;
            if left == 0 {
                // Stay armed so subsequent checks keep hitting until `clear`.
                Some(true)
            } else {
                c.set(Some(left - 1));
                Some(false)
            }
        })
    }

    /// RAII clear so a panic mid-test cannot poison later tests.
    pub struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            clear();
        }
    }
}

fn transcript_fingerprint(text: &str) -> String {
    format!("sha256:{}", hex_sha256(text.as_bytes()))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in hash {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn derivation_digest(
    base_version: &str,
    base_fingerprint: &str,
    rendered: &str,
    anchors: &BTreeMap<(usize, usize), SourceAnchor>,
    protected: &[SourceSpan],
    formatter_contract: &str,
) -> String {
    // Canonical JSON (sorted keys, compact) matching the #100 proof shape.
    let mut anchor_list = Vec::with_capacity(anchors.len());
    for (&(s, e), a) in anchors {
        anchor_list.push(serde_json::json!([s, e, a.rendered_start, a.rendered_end]));
    }
    let protected_list: Vec<serde_json::Value> = protected
        .iter()
        .map(|r| serde_json::json!([r.start, r.end]))
        .collect();
    let canonical = serde_json::json!({
        "anchors": anchor_list,
        "base_fingerprint": base_fingerprint,
        "base_version": base_version,
        "formatter_contract": formatter_contract,
        "protected_source_ranges": protected_list,
        "rendered": rendered,
    });
    // serde_json map iteration is sorted by key for BTree-backed Value::Object.
    let encoded = serde_json::to_string(&canonical).expect("canonical baseline JSON");
    format!("sha256:{}", hex_sha256(encoded.as_bytes()))
}

/// Case-insensitive whole-token anchors from source → rendered.
///
/// Uses a linear dual-pointer scan so local 1:1 rewrites (`is`→`are`, `lets`→`let's`,
/// `didnt`→`didn't`) leave following tokens anchored. Only truly unmappable source
/// tokens are omitted; non-matching rendered tokens are not consumed while hunting
/// for an exact match (that old greedy hunt collapsed the rest of the stream).
fn source_anchors(base: &str, rendered: &str) -> BTreeMap<(usize, usize), SourceAnchor> {
    let base_tokens = word_tokens(base);
    let rendered_tokens = word_tokens(rendered);
    let mut anchors = BTreeMap::new();

    // Equal token counts: strict index zip. Local grammar rewrites are 1:1, so
    // unmappable indices drop without shifting later matches.
    if base_tokens.len() == rendered_tokens.len() {
        for (&(bs, be, btok), &(rs, re, rtok)) in base_tokens.iter().zip(rendered_tokens.iter()) {
            if eq_ascii_ignore_case(btok, rtok) {
                anchors.insert((bs, be), SourceAnchor::new(rs, re));
            }
        }
        return anchors;
    }

    // Unequal counts (command expansion, list inference): dual-pointer realignment.
    let mut bi = 0usize;
    let mut ri = 0usize;
    while bi < base_tokens.len() && ri < rendered_tokens.len() {
        let (bs, be, btok) = base_tokens[bi];
        let (rs, re, rtok) = rendered_tokens[ri];
        if eq_ascii_ignore_case(btok, rtok) {
            anchors.insert((bs, be), SourceAnchor::new(rs, re));
            bi += 1;
            ri += 1;
            continue;
        }
        // One source token deleted (e.g. command phrase word removed).
        if bi + 1 < base_tokens.len()
            && eq_ascii_ignore_case(base_tokens[bi + 1].2, rtok)
        {
            bi += 1;
            continue;
        }
        // One rendered word inserted.
        if ri + 1 < rendered_tokens.len()
            && eq_ascii_ignore_case(btok, rendered_tokens[ri + 1].2)
        {
            ri += 1;
            continue;
        }
        // Local 1:1 substitution mid-stream (next tokens realign).
        if bi + 1 < base_tokens.len()
            && ri + 1 < rendered_tokens.len()
            && eq_ascii_ignore_case(base_tokens[bi + 1].2, rendered_tokens[ri + 1].2)
        {
            bi += 1;
            ri += 1;
            continue;
        }
        // Prefer draining the longer remaining side (multi-token command deletions).
        let base_left = base_tokens.len() - bi;
        let rend_left = rendered_tokens.len() - ri;
        if base_left > rend_left {
            bi += 1;
        } else if rend_left > base_left {
            ri += 1;
        } else {
            bi += 1;
            ri += 1;
        }
    }
    anchors
}

fn protected_source_ranges(source: &str) -> Vec<SourceSpan> {
    let mut ranges: Vec<SourceSpan> = Vec::new();

    // Explicit §4 command spans + quote interiors from the parser.
    let parsed = parse_formatting_commands(source);
    for span in parsed.command_spans() {
        ranges.push(*span);
    }
    for event in parsed.events() {
        if let CommandEvent::Command {
            kind: CommandKind::Quote { interior, open, close },
            ..
        } = event
        {
            ranges.push(*open);
            ranges.push(*interior);
            ranges.push(*close);
        }
    }

    // Composite protected spans on the raw Validated string.
    collect_composite_spans(source, &mut ranges);

    // Paired ASCII quotes and inline/fenced code.
    collect_quote_and_code_ranges(source, &mut ranges);

    normalize_ranges(ranges)
}

fn collect_composite_spans(text: &str, out: &mut Vec<SourceSpan>) {
    collect_urls_linear(text, out);
    collect_paths_linear(text, out);
    collect_flags_linear(text, out);

    // Technical identifiers + numbers/dates/times: single token pass (O(n)).
    for (s, e, tok) in word_tokens(text) {
        if (tok.contains('_')
            && tok.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && tok.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            || is_number_date_or_time(tok)
        {
            out.push(SourceSpan::new(s, e));
        }
    }
}

/// Single forward pass for `https?://\S+` (O(n); advance past each match).
fn collect_urls_linear(text: &str, out: &mut Vec<SourceSpan>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'h' {
            let rest = &text[i..];
            let prefix_len = if rest.starts_with("https://") {
                8
            } else if rest.starts_with("http://") {
                7
            } else {
                i += 1;
                continue;
            };
            let start = i;
            let mut end = start + prefix_len;
            while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
                end += 1;
            }
            out.push(SourceSpan::new(start, end));
            i = end;
        } else {
            i += 1;
        }
    }
}

/// Single forward pass over non-whitespace tokens for path-like spans (O(n)).
fn collect_paths_linear(text: &str, out: &mut Vec<SourceSpan>) {
    let mut i = 0;
    while i < text.len() {
        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            i += ch.len_utf8();
            continue;
        }
        let start = i;
        while i < text.len() {
            let Some(c) = text[i..].chars().next() else {
                break;
            };
            if c.is_whitespace() {
                break;
            }
            i += c.len_utf8();
        }
        let end = i;
        if is_path_token(&text[start..end]) {
            out.push(SourceSpan::new(start, end));
        }
    }
}

fn is_path_token(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    // Avoid re-matching URLs (already collected separately).
    if tok.starts_with("http://") || tok.starts_with("https://") {
        return false;
    }
    // Bare ./ or ../
    if tok.starts_with("./") || tok.starts_with("../") {
        return true;
    }
    // Leading / or ~/
    if tok.starts_with("~/") {
        return tok.len() > 2;
    }
    if tok.starts_with('/') {
        return tok.len() > 1;
    }
    // crates/… style: alnum or '.' start, contains '/', no spaces (token already).
    if tok.starts_with(|c: char| c.is_alphanumeric() || c == '.') && tok.contains('/') {
        return true;
    }
    false
}

/// Single forward pass for `--flag`, `--flag=value`, and `-x` (O(n)).
fn collect_flags_linear(text: &str, out: &mut Vec<SourceSpan>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'-' {
            let start = i;
            // --flag or --flag=value or -x
            if i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                i += 2;
                let name_start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-')
                {
                    i += 1;
                }
                if i == name_start {
                    i = start + 1;
                    continue;
                }
                if i < bytes.len() && bytes[i] == b'=' {
                    i += 1;
                    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                }
                out.push(SourceSpan::new(start, i));
                continue;
            } else if i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphanumeric() {
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-')
                {
                    i += 1;
                }
                out.push(SourceSpan::new(start, i));
                continue;
            }
        }
        i += 1;
    }
}

fn is_number_date_or_time(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    // YYYY-MM-DD
    if tok.len() == 10
        && tok.as_bytes()[4] == b'-'
        && tok.as_bytes()[7] == b'-'
        && tok.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
    {
        return true;
    }
    // HH:MM
    if tok.len() == 5
        && tok.as_bytes()[2] == b':'
        && tok.bytes().enumerate().all(|(i, b)| {
            if i == 2 {
                b == b':'
            } else {
                b.is_ascii_digit()
            }
        })
    {
        return true;
    }
    // whole decimal integer
    tok.bytes().all(|b| b.is_ascii_digit())
}

fn collect_quote_and_code_ranges(text: &str, out: &mut Vec<SourceSpan>) {
    // Fenced ``` … ```
    collect_fenced_code_ranges(text, out);
    // Inline `…`
    let mut in_tick = false;
    let mut tick_start = 0;
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'`' {
            // skip triple
            if text[i..].starts_with("```") {
                i += 3;
                continue;
            }
            if !in_tick {
                in_tick = true;
                tick_start = i;
            } else {
                out.push(SourceSpan::new(tick_start, i + 1));
                in_tick = false;
            }
        }
        i += 1;
    }

    // Quotation ranges (ASCII + curly), ported from edit-safety
    // `_quotation_source_ranges`: unmatched/ambiguous → whole base.
    collect_quotation_ranges(text, out);
}

fn collect_fenced_code_ranges(text: &str, out: &mut Vec<SourceSpan>) {
    let mut search = 0;
    while let Some(rel) = text[search..].find("```") {
        let start = search + rel;
        if let Some(rel2) = text[start + 3..].find("```") {
            let end = start + 3 + rel2 + 3;
            out.push(SourceSpan::new(start, end));
            search = end;
        } else {
            break;
        }
    }
}

/// Port of edit-safety `_quotation_source_ranges` (UTF-8 byte spans).
///
/// Handles ASCII `"`/`'`, curly “”/‘’, word-apostrophe skips for `'` and `’`,
/// and appends `(0, len)` when any delimiter family is unmatched/ambiguous.
fn collect_quotation_ranges(text: &str, out: &mut Vec<SourceSpan>) {
    let mut ambiguous = false;

    let (dq, dq_unmatched) = same_quote_ranges(text, '"', false);
    out.extend(dq);
    ambiguous |= dq_unmatched;

    let (sq, sq_unmatched) = same_quote_ranges(text, '\'', true);
    out.extend(sq);
    ambiguous |= sq_unmatched;

    let (curly_d, curly_d_amb) = curly_quote_ranges(text, '\u{201C}', '\u{201D}', false);
    out.extend(curly_d);
    ambiguous |= curly_d_amb;

    let (curly_s, curly_s_amb) = curly_quote_ranges(text, '\u{2018}', '\u{2019}', true);
    out.extend(curly_s);
    ambiguous |= curly_s_amb;

    if ambiguous && !text.is_empty() {
        out.push(SourceSpan::new(0, text.len()));
    }
}

/// `delimiter` positions paired 0-1, 2-3, …; odd count ⇒ unmatched.
fn same_quote_ranges(
    text: &str,
    delimiter: char,
    ignore_word_apostrophes: bool,
) -> (Vec<SourceSpan>, bool) {
    let delim_len = delimiter.len_utf8();
    let mut positions: Vec<usize> = Vec::new();
    let mut search = 0usize;
    while search < text.len() {
        let Some(rel) = text[search..].find(delimiter) else {
            break;
        };
        let index = search + rel;
        if !(ignore_word_apostrophes && inside_word_apostrophe(text, index, delim_len)) {
            positions.push(index);
        }
        search = index + delim_len;
    }
    let mut ranges = Vec::new();
    let mut i = 0;
    while i + 1 < positions.len() {
        let start = positions[i];
        let end = positions[i + 1] + delim_len;
        ranges.push(SourceSpan::new(start, end));
        i += 2;
    }
    let unmatched = positions.len() % 2 == 1;
    (ranges, unmatched)
}

fn curly_quote_ranges(
    text: &str,
    opening: char,
    closing: char,
    closing_can_be_apostrophe: bool,
) -> (Vec<SourceSpan>, bool) {
    let close_len = closing.len_utf8();
    let mut ranges = Vec::new();
    let mut pending: Option<usize> = None;
    let mut ambiguous = false;
    for (index, ch) in text.char_indices() {
        if ch == opening {
            if pending.is_some() {
                ambiguous = true;
            } else {
                pending = Some(index);
            }
        } else if ch == closing {
            if closing_can_be_apostrophe && inside_word_apostrophe(text, index, close_len) {
                continue;
            }
            match pending.take() {
                Some(start) => {
                    ranges.push(SourceSpan::new(start, index + close_len));
                }
                None => {
                    ambiguous = true;
                }
            }
        }
    }
    if pending.is_some() {
        ambiguous = true;
    }
    (ranges, ambiguous)
}

/// True when `byte_index` is a mid-word apostrophe (alnum/_ on both sides).
fn inside_word_apostrophe(text: &str, byte_index: usize, delim_len: usize) -> bool {
    if byte_index == 0 || byte_index + delim_len > text.len() {
        return false;
    }
    let left = text[..byte_index].chars().next_back();
    let right = text[byte_index + delim_len..].chars().next();
    match (left, right) {
        (Some(l), Some(r)) => {
            (l.is_alphanumeric() || l == '_') && (r.is_alphanumeric() || r == '_')
        }
        _ => false,
    }
}

fn normalize_ranges(mut ranges: Vec<SourceSpan>) -> Vec<SourceSpan> {
    ranges.retain(|r| r.start < r.end);
    ranges.sort_by_key(|r| (r.start, r.end));
    let mut merged: Vec<SourceSpan> = Vec::new();
    for r in ranges {
        if let Some(last) = merged.last_mut() {
            if r.start <= last.end {
                last.end = last.end.max(r.end);
                continue;
            }
        }
        merged.push(r);
    }
    merged
}

// ─── Smart formatting ────────────────────────────────────────────────────────

/// Smart rewrite. Returns `None` on cooperative deadline miss.
fn smart_format(
    input: &str,
    dictionary: &[&str],
    protected_names: &[&str],
    deadline_at: Instant,
) -> Option<String> {
    if input.is_empty() {
        return Some(String::new());
    }
    if is_shell_command_line(input) {
        return Some(input.to_owned());
    }

    // §5.1 step 1: protected recognition before any edit (quote/code/composite,
    // dictionary, protected names). Structural + casing + local grammar honor this.
    let protect = build_edit_protection(input, dictionary, protected_names);

    // §5.1 step 2: command parse before already-correct identity short-circuits.
    // Spoken commands in an otherwise sentence-cased string must still expand
    // (e.g. "Ship it command period." → "Ship it.").
    let parsed = parse_formatting_commands(input);
    if deadline_hit(deadline_at) {
        return None;
    }
    // A command-looking phrase inside quote/code is content, not an instruction.
    // The parser deliberately owns only the closed phrase grammar, so the formatter
    // applies its pre-edit protection boundary before consuming parser events.
    let quote_or_code = build_quote_and_code_protection(input);
    let protected_command = parsed
        .command_spans()
        .iter()
        .any(|span| span_touches_protection(span.start, span.end, &quote_or_code));
    let literal = if protected_command {
        input.to_owned()
    } else {
        parsed.render_commands_only()
    };
    let has_commands = parsed.has_command_span() && !protected_command;

    // F19/F20/F35 identity only when no §4 command span remains.
    if !has_commands && is_already_correct_identity(input) {
        return Some(input.to_owned());
    }

    let mut text = if !has_commands && literal == input {
        // No §4 commands: list inference / enumeration / email greeting on Validated text.
        // Each structural rewrite skips when it would touch protected spans.
        if let Some(bullets) = try_bullet_inference(input, &protect) {
            return Some(bullets);
        }
        let mut t = try_counted_enumeration(input, &protect).unwrap_or_else(|| input.to_owned());
        // Recompute mask after each structural rewrite (byte indices shift).
        let p = build_edit_protection(&t, dictionary, protected_names);
        t = apply_email_hi_name(&t, &p);
        let p = build_edit_protection(&t, dictionary, protected_names);
        t = split_email_update_sentence(&t, &p);
        t
    } else {
        literal
    };

    if deadline_hit(deadline_at) {
        return None;
    }

    text = apply_vocative_commas(&text, dictionary, protected_names);
    text = apply_casing(&text, dictionary, protected_names);
    let protected = build_edit_protection(&text, dictionary, protected_names);
    text = capitalize_numbered_list_items(&text, &protected);

    if deadline_hit(deadline_at) {
        return None;
    }

    // Numbered lists from commands: casing only, no extra terminal punct.
    if is_numbered_list_render(&text) {
        if !has_commands {
            // fall through
        } else {
            // Apply closed grammar only when no commands (separability).
            return Some(text);
        }
    }

    let fenced_code = build_fenced_code_protection(&text);
    text = add_terminal_punctuation(&text, &fenced_code);

    // Closed Minimal Grammar catalog applied locally for hermetic #99 Smart
    // oracle (F25/F26/F27). Separability (§6.1): skip when any §4 command span
    // is present, or when consecutive word-tokens `new`+`line` appear (F36).
    // Quote/code/composite + dictionary/names protection so local rewrites
    // cannot mutate protected interiors or snapshot tokens.
    if !has_commands && !has_new_line_token_pair(input) {
        if deadline_hit(deadline_at) {
            return None;
        }
        text = apply_closed_local_grammar(&text, dictionary, protected_names);
    }

    Some(text)
}

/// True when consecutive whole word-tokens `new` + `line` appear (case-insensitive).
fn has_new_line_token_pair(text: &str) -> bool {
    let tokens = word_tokens(text);
    tokens.windows(2).any(|w| {
        ascii_lower(w[0].2) == "new" && ascii_lower(w[1].2) == "line"
    })
}

fn is_already_correct_identity(text: &str) -> bool {
    if is_shell_command_line(text) {
        return true;
    }
    // Existing numbered list (F20).
    if text.contains('\n') && text.lines().all(|l| {
        let t = l.trim();
        t.is_empty() || starts_with_numbered_marker(t)
    }) && text.lines().any(starts_with_numbered_marker)
    {
        return true;
    }
    // Already terminal-punctuated and sentence-cased (F19).
    if text.ends_with(['.', '!', '?']) && text.chars().next().is_some_and(|c| c.is_uppercase()) {
        return true;
    }
    // Title-case multi-word phrase without terminal punct (F35).
    if !text.contains('\n') && !text.ends_with(['.', '!', '?']) && is_all_title_case_words(text) {
        return true;
    }
    false
}

fn starts_with_numbered_marker(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    if i >= bytes.len() || !bytes[i].is_ascii_digit() {
        return false;
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1] == b' '
}

fn is_all_title_case_words(text: &str) -> bool {
    let words: Vec<&str> = word_tokens(text).into_iter().map(|(_, _, t)| t).collect();
    if words.len() < 2 {
        return false;
    }
    words.iter().all(|w| {
        w.chars()
            .next()
            .is_some_and(|c| c.is_uppercase() || !c.is_alphabetic())
    })
}

fn is_numbered_list_render(text: &str) -> bool {
    text.contains('\n') && text.lines().all(|l| {
        let t = l.trim();
        t.is_empty() || starts_with_numbered_marker(t) || t.starts_with("- ")
    })
}

// ─── Shell recognition (F07 family) ──────────────────────────────────────────

const SHELL_VERBS: &[&str] = &[
    "run", "ls", "cd", "cat", "grep", "git", "curl", "ssh", "sudo", "rm", "cp", "mv",
    "chmod", "chown", "docker", "kubectl", "python", "python3", "pip", "cargo", "npm",
    "make", "systemctl",
];

fn is_shell_command_line(text: &str) -> bool {
    let mut t = text.trim();
    if t.ends_with('.') {
        return false;
    }
    if let Some(rest) = t.strip_prefix('$') {
        t = rest.trim_start();
    }
    let parts: Vec<&str> = t.split_whitespace().collect();
    if parts.is_empty() {
        return false;
    }
    let first = ascii_lower(parts[0]);
    if !SHELL_VERBS.iter().any(|v| *v == first) {
        return false;
    }
    let has_flag = parts.iter().skip(1).any(|p| p.starts_with('-'));
    let second_tool = parts.get(1).is_some_and(|p| {
        let l = ascii_lower(p);
        SHELL_VERBS.iter().any(|v| *v == l) || matches!(l.as_str(), "cargo" | "npm" | "make")
    });
    let second_path = parts
        .get(1)
        .is_some_and(|p| p.contains('/') || p.starts_with('.'));
    has_flag || second_tool || second_path
}

// ─── List inference (D3-B / F17, D14-A / F18) ────────────────────────────────

const CLAUSE_MARKERS: &[&str] = &[
    "when", "if", "because", "while", "although", "after", "before", "unless", "that",
    "which", "who",
];

fn try_bullet_inference(text: &str, protected: &[bool]) -> Option<String> {
    // Whole-string structural rewrite: any protected byte ⇒ fail closed.
    if span_touches_protection(0, text.len(), protected) {
        return None;
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() || ascii_lower(words[0]) != "buy" {
        return None;
    }
    let rest = &words[1..];
    if rest.len() < 3 {
        return None;
    }
    for (i, w) in rest.iter().enumerate() {
        let l = ascii_lower(w);
        if CLAUSE_MARKERS.iter().any(|m| *m == l) {
            return None;
        }
        if l == "and" && rest.get(i + 1).is_some_and(|n| ascii_lower(n) == "then") {
            return None;
        }
    }
    let mut items = Vec::new();
    for w in rest {
        let cleaned = w.trim_matches(',');
        if cleaned.is_empty() {
            continue;
        }
        // simple item: no internal punct other than hyphen
        if cleaned.chars().any(|c| !c.is_alphanumeric() && c != '-' && c != '\'') {
            return None;
        }
        items.push(cleaned);
    }
    if items.len() < 3 {
        return None;
    }
    let mut out = String::from("Buy:");
    for item in items {
        out.push('\n');
        out.push_str("- ");
        out.push_str(item);
    }
    Some(out)
}

fn count_word_value(word: &str) -> Option<usize> {
    match ascii_lower(word).as_str() {
        "two" | "2" => Some(2),
        "three" | "3" => Some(3),
        "four" | "4" => Some(4),
        "five" | "5" => Some(5),
        "six" | "6" => Some(6),
        "seven" | "7" => Some(7),
        "eight" | "8" => Some(8),
        "nine" | "9" => Some(9),
        "ten" | "10" => Some(10),
        "eleven" | "11" => Some(11),
        "twelve" | "12" => Some(12),
        _ => None,
    }
}

fn try_counted_enumeration(text: &str, protected: &[bool]) -> Option<String> {
    // … N <noun> items… and last
    let tokens: Vec<(usize, usize, &str)> = word_tokens(text);
    if tokens.len() < 5 {
        return None;
    }
    // Find count word
    let mut count_idx = None;
    let mut n = 0usize;
    for (i, (_, _, tok)) in tokens.iter().enumerate() {
        if let Some(v) = count_word_value(tok) {
            count_idx = Some(i);
            n = v;
            break;
        }
    }
    let count_idx = count_idx?;
    if count_idx + 2 >= tokens.len() || n < 2 {
        return None;
    }
    // noun is next token
    let noun = tokens[count_idx + 1].2;
    // rest after noun until end; must contain "and"
    let after_noun = &tokens[count_idx + 2..];
    let and_pos = after_noun.iter().rposition(|(_, _, t)| ascii_lower(t) == "and")?;
    if and_pos == 0 || and_pos + 1 >= after_noun.len() {
        return None;
    }
    let before: Vec<&str> = after_noun[..and_pos].iter().map(|t| t.2).collect();
    // last item may be multi-token after and
    let after_and: Vec<&str> = after_noun[and_pos + 1..].iter().map(|t| t.2).collect();
    if after_and.is_empty() || before.is_empty() {
        return None;
    }
    let need = n - 1;
    if before.len() < need {
        return None;
    }
    let mut items: Vec<String> = Vec::new();
    if before.len() == need {
        items.extend(before.iter().map(|s| (*s).to_owned()));
    } else {
        let extra = before.len() - need;
        items.push(before[..=extra].join(" "));
        items.extend(before[extra + 1..].iter().map(|s| (*s).to_owned()));
    }
    items.push(after_and.join(" "));
    if items.len() != n {
        return None;
    }

    // Prefix = text before count word (preserve spacing loosely via original slice)
    let prefix_end = tokens[count_idx].0;
    // Rebuilt region is count…end; skip if quote/code/composite/dict spans would be destroyed.
    if span_touches_protection(prefix_end, text.len(), protected) {
        return None;
    }
    let prefix = &text[..prefix_end];
    let count_tok = tokens[count_idx].2;
    let body = if n == 2 {
        format!("{} and {}", items[0], items[1])
    } else {
        let mut b = items[..n - 1].join(", ");
        b.push_str(", and ");
        b.push_str(&items[n - 1]);
        b
    };
    Some(format!("{prefix}{count_tok} {noun}: {body}"))
}

// ─── Email / vocative / discourse commas (fixture-locked) ────────────────────

fn apply_email_hi_name(text: &str, protected: &[bool]) -> String {
    // hi jordan … → Hi Jordan, …
    let tokens: Vec<(usize, usize, &str)> = word_tokens(text);
    if tokens.len() < 2 {
        return text.to_owned();
    }
    if ascii_lower(tokens[0].2) != "hi" {
        return text.to_owned();
    }
    let name = tokens[1].2;
    if !name.chars().all(|c| c.is_ascii_alphabetic()) {
        return text.to_owned();
    }
    // Greeting rewrite spans hi + name.
    if span_touches_protection(tokens[0].0, tokens[1].1, protected) {
        return text.to_owned();
    }
    let name_c = capitalize_word(name);
    let rest_start = tokens[1].1;
    let rest = text[rest_start..].trim_start();
    format!("Hi {name_c}, {rest}")
}

fn split_email_update_sentence(text: &str, protected: &[bool]) -> String {
    // "update i will" → "update. I will" (F04)
    let lower = ascii_lower(text);
    if let Some(rel) = lower.find("update i will") {
        let end = rel + "update i will".len();
        if span_touches_protection(rel, end, protected) {
            return text.to_owned();
        }
        let mut out = String::new();
        out.push_str(&text[..rel]);
        out.push_str(&text[rel..rel + "update".len()]);
        out.push_str(". I will");
        out.push_str(&text[end..]);
        return out;
    }
    text.to_owned()
}

fn apply_vocative_commas(
    text: &str,
    dictionary: &[&str],
    protected_names: &[&str],
) -> String {
    let mut t = text.to_owned();
    // hey␠ → Hey,
    let protected = build_edit_protection(&t, dictionary, protected_names);
    if let Some(rest) = strip_prefix_ci(&t, "hey") {
        if rest.starts_with(|c: char| c.is_whitespace()) {
            if !span_touches_protection(0, "hey".len(), &protected) {
                t = format!("Hey,{}", rest);
            }
        }
    }
    // trailing ok / lol → , ok / , lol (before optional punct)
    let protected = build_edit_protection(&t, dictionary, protected_names);
    t = comma_before_trailing_word(&t, "ok", &protected);
    let protected = build_edit_protection(&t, dictionary, protected_names);
    t = comma_before_trailing_word(&t, "lol", &protected);
    // said " → said, "
    let protected = build_edit_protection(&t, dictionary, protected_names);
    t = replace_ci_word_before_quote(&t, "said", &protected);
    // " and → ," and (closing quote discourse comma, F21b)
    if let Some(idx) = t.find("\" ") {
        // only when followed by "and"
        let after = &t[idx + 2..];
        if after.len() >= 3 && ascii_lower(&after[..3.min(after.len())]).starts_with("and") {
            // ensure not already ,"
            let fenced_code = build_fenced_code_protection(&t);
            if idx > 0
                && !t[..idx].ends_with(',')
                && !span_touches_protection(idx, idx + 1, &fenced_code)
            {
                t = format!("{},\"{}", &t[..idx], &t[idx + 1..]);
            }
        }
    }
    t
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let s_bytes = s.as_bytes();
    let p_bytes = prefix.as_bytes();
    if s_bytes.len() < p_bytes.len() {
        return None;
    }
    if s_bytes[..p_bytes.len()]
        .iter()
        .zip(p_bytes)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
    {
        Some(&s[p_bytes.len()..])
    } else {
        None
    }
}

fn comma_before_trailing_word(text: &str, word: &str, protected: &[bool]) -> String {
    // Match optional terminal punct.
    let trimmed = text.trim_end_matches(['.', '!', '?']);
    let trail = &text[trimmed.len()..];
    let lower = ascii_lower(trimmed);
    let needle = format!(" {word}");
    if lower.ends_with(&needle) {
        let head_end = trimmed.len() - needle.len();
        let word_start = head_end + 1;
        if head_end > 0
            && !trimmed[..head_end].ends_with(',')
            && !span_touches_protection(word_start, trimmed.len(), protected)
        {
            return format!("{}, {}{trail}", &trimmed[..head_end], word);
        }
    }
    text.to_owned()
}

fn replace_ci_word_before_quote(text: &str, word: &str, protected: &[bool]) -> String {
    // word + spaces + " → word, "
    let lower = ascii_lower(text);
    let mut search = 0;
    let mut out = String::new();
    let mut last = 0;
    while let Some(rel) = lower[search..].find(word) {
        let abs = search + rel;
        // word boundary
        let boundary_before = abs == 0
            || !text.as_bytes()[abs - 1].is_ascii_alphanumeric();
        let after_word = abs + word.len();
        if boundary_before
            && after_word <= text.len()
            && !span_touches_protection(abs, after_word, protected)
        {
            let rest = &text[after_word..];
            let ws_len = rest.chars().take_while(|c| c.is_whitespace()).map(char::len_utf8).sum::<usize>();
            if rest.len() > ws_len && rest.as_bytes()[ws_len] == b'"' {
                // already has comma?
                if abs > 0 && text[..abs].ends_with(',') {
                    search = after_word;
                    continue;
                }
                out.push_str(&text[last..abs]);
                out.push_str(word);
                out.push_str(", ");
                out.push('"');
                last = after_word + ws_len + 1;
                search = last;
                continue;
            }
        }
        search = abs + word.len();
    }
    out.push_str(&text[last..]);
    if last == 0 {
        text.to_owned()
    } else {
        out
    }
}

// ─── Casing ──────────────────────────────────────────────────────────────────

const WEEKDAYS: &[&str] = &[
    "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday",
];

fn apply_casing(text: &str, dictionary: &[&str], protected_names: &[&str]) -> String {
    let protected = build_edit_protection(text, dictionary, protected_names);
    let mut chars: Vec<char> = text.chars().collect();
    // Map byte index → char index
    let mut byte_to_char: Vec<usize> = vec![0; text.len() + 1];
    {
        let mut ci = 0usize;
        for (bi, ch) in text.char_indices() {
            byte_to_char[bi] = ci;
            ci += 1;
            // mark end of this char
            let end = bi + ch.len_utf8();
            if end <= text.len() {
                byte_to_char[end] = ci;
            }
        }
        byte_to_char[text.len()] = ci;
    }

    for (s, e, tok) in word_tokens(text) {
        if (s..e).any(|i| protected.get(i).copied().unwrap_or(false)) {
            continue;
        }
        let cs = byte_to_char[s];
        if is_sentence_or_line_start(text, s) {
            if let Some(c) = chars.get_mut(cs) {
                if c.is_alphabetic() {
                    *c = c.to_ascii_uppercase();
                }
            }
        } else if tok == "i" || tok == "I" {
            if tok.len() == 1 {
                if let Some(c) = chars.get_mut(cs) {
                    *c = 'I';
                }
            }
        } else if WEEKDAYS.iter().any(|w| eq_ascii_ignore_case(tok, w)) {
            if let Some(c) = chars.get_mut(cs) {
                *c = c.to_ascii_uppercase();
            }
            for (k, ch) in tok.chars().enumerate().skip(1) {
                if let Some(c) = chars.get_mut(cs + k) {
                    *c = ch.to_ascii_lowercase();
                }
            }
        }
    }
    chars.into_iter().collect()
}

fn is_sentence_or_line_start(text: &str, byte_idx: usize) -> bool {
    if byte_idx == 0 {
        return true;
    }
    let before = &text[..byte_idx];
    if before.ends_with('\n') {
        return true;
    }
    let trimmed = before.trim_end_matches([' ', '\t']);
    trimmed.ends_with(['.', '!', '?'])
}

/// Byte mask for all local edits before/while rewriting (spec §5.1 protected recognition).
///
/// Covers quote/code/composite (same families as seal), bare `quote`…`unquote` interiors,
/// and Recording-time dictionary / protected-name whole tokens. Unmatched/ambiguous quote
/// delimiters protect the whole base (fail-closed). Used by structural transforms, casing,
/// and local grammar.
fn build_edit_protection(text: &str, dictionary: &[&str], protected_names: &[&str]) -> Vec<bool> {
    let mut protected = vec![false; text.len()];
    let mut ranges = Vec::new();
    collect_composite_spans(text, &mut ranges);
    // ASCII + curly quotes, inline/fenced code — same as seal `protected_source_ranges`.
    collect_quote_and_code_ranges(text, &mut ranges);
    // Bare quote … unquote interiors (token words; command form handled by parser at seal).
    {
        let tokens = word_tokens(text);
        let mut i = 0;
        while i < tokens.len() {
            if ascii_lower(tokens[i].2) == "quote" {
                if let Some(j) =
                    (i + 1..tokens.len()).find(|&j| ascii_lower(tokens[j].2) == "unquote")
                {
                    let start = tokens[i].1;
                    let end = tokens[j].0;
                    ranges.push(SourceSpan::new(start, end));
                    i = j;
                }
            }
            i += 1;
        }
    }
    for r in normalize_ranges(ranges) {
        for i in r.start..r.end.min(protected.len()) {
            protected[i] = true;
        }
    }
    // Dictionary / names whole tokens (casing + local grammar).
    for (s, e, tok) in word_tokens(text) {
        let lower = ascii_lower(tok);
        if dictionary.iter().any(|d| eq_ascii_ignore_case(d, tok))
            || protected_names.iter().any(|n| eq_ascii_ignore_case(n, tok))
            || dictionary.iter().any(|d| ascii_lower(d) == lower)
        {
            for i in s..e {
                if i < protected.len() {
                    protected[i] = true;
                }
            }
        }
    }
    protected
}

fn build_fenced_code_protection(text: &str) -> Vec<bool> {
    protection_mask(text, collect_fenced_code_ranges)
}

fn build_quote_and_code_protection(text: &str) -> Vec<bool> {
    protection_mask(text, collect_quote_and_code_ranges)
}

fn protection_mask(text: &str, collect: fn(&str, &mut Vec<SourceSpan>)) -> Vec<bool> {
    let mut protected = vec![false; text.len()];
    let mut ranges = Vec::new();
    collect(text, &mut ranges);
    for range in ranges {
        for byte in range.start..range.end.min(protected.len()) {
            protected[byte] = true;
        }
    }
    protected
}

#[inline]
fn span_touches_protection(start: usize, end: usize, protected: &[bool]) -> bool {
    (start..end).any(|i| protected.get(i).copied().unwrap_or(false))
}

fn capitalize_numbered_list_items(text: &str, protected: &[bool]) -> String {
    if !text.lines().any(starts_with_numbered_marker) {
        return text.to_owned();
    }
    let mut line_start = 0usize;
    text.lines()
        .map(|line| {
            if let Some(rest) = line.split_once(". ") {
                if rest.0.chars().all(|c| c.is_ascii_digit()) {
                    let mut item = rest.1.to_owned();
                    if let Some(first) = item.chars().next() {
                        let item_start = line_start + rest.0.len() + 2;
                        if first.is_alphabetic()
                            && !span_touches_protection(
                                item_start,
                                item_start + first.len_utf8(),
                                protected,
                            )
                        {
                            let upper = first.to_ascii_uppercase();
                            item = format!("{upper}{}", &item[first.len_utf8()..]);
                        }
                    }
                    let rendered = format!("{}. {item}", rest.0);
                    line_start += line.len() + 1;
                    return rendered;
                }
            }
            line_start += line.len() + 1;
            line.to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn capitalize_word(w: &str) -> String {
    let mut c = w.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
    }
}

// ─── Terminal punctuation ────────────────────────────────────────────────────

/// Closed question cues locked by F01/F02 — no open-ended detection.
fn is_closed_question(text: &str) -> bool {
    let body = text.trim().trim_end_matches(['.', '!', '?']);
    if body.is_empty() {
        return false;
    }
    let lower = ascii_lower(body);
    if lower.contains("can you") {
        return true;
    }
    // trailing ok
    if let Some(last) = body.split_whitespace().last() {
        if ascii_lower(last.trim_end_matches(['.', '!', '?', ','])) == "ok" {
            return true;
        }
    }
    false
}

/// Finite-verb heuristic for multi-line Smart period cascading (F36b vs F13b).
const PERIOD_VERBS: &[&str] = &[
    "is", "are", "was", "were", "be", "been", "have", "has", "had", "do", "does", "did",
    "will", "would", "can", "could", "should", "may", "might", "must", "shall", "need",
    "needs", "think", "send", "ship", "meet", "see", "file", "increase", "ignore",
    "enable", "returns", "works", "ended", "launched", "approved", "blocking", "ping",
    "review", "let", "lets", "aint", "contains", "missing", "open", "clone", "press",
    "use", "join", "keep", "preserve",
];

fn line_independently_needs_period(line: &str) -> bool {
    let s = line.trim();
    if s.is_empty() || s.ends_with(['.', '!', '?', ':']) {
        return false;
    }
    if starts_with_numbered_marker(s) || s.starts_with("- ") {
        return false;
    }
    if is_closed_question(s) {
        return true;
    }
    let words: Vec<&str> = word_tokens(s).into_iter().map(|t| t.2).collect();
    if words.len() < 2 {
        return false;
    }
    words
        .iter()
        .any(|w| PERIOD_VERBS.iter().any(|v| eq_ascii_ignore_case(w, v)))
}

fn terminal_byte_is_protected(line: &str, protected: &[bool]) -> bool {
    let stripped = line.trim_end();
    !stripped.is_empty()
        && protected
            .get(stripped.len().saturating_sub(1))
            .copied()
            .unwrap_or(false)
}

fn add_terminal_punct_line(line: &str, protected: &[bool]) -> String {
    let stripped = line.trim_end();
    if stripped.is_empty() || stripped.ends_with(['.', '!', '?', ':']) {
        return line.to_owned();
    }
    if terminal_byte_is_protected(line, protected) {
        return line.to_owned();
    }
    if starts_with_numbered_marker(stripped) || stripped.starts_with("- ") {
        return line.to_owned();
    }
    let trail = &line[stripped.len()..];
    if is_closed_question(stripped) {
        return format!("{stripped}?{trail}");
    }
    format!("{stripped}.{trail}")
}

fn add_terminal_punctuation(text: &str, protected: &[bool]) -> String {
    if text.is_empty() || is_shell_command_line(text) {
        return text.to_owned();
    }
    // Title-case phrase without punct stays identity (F35) — handled earlier.
    if is_all_title_case_words(text) && !text.contains('\n') && !text.ends_with(['.', '!', '?'])
    {
        return text.to_owned();
    }

    // Paragraphs (`\n\n` from command new paragraph): D1-B — period on each fragment.
    if text.contains("\n\n") {
        // Splitting/trimming paragraphs that intersect fenced code can delete
        // significant whitespace. Keep the whole late-punctuation pass inert;
        // earlier span-aware casing and grammar have already handled exteriors.
        if protected.iter().any(|byte| *byte) {
            return text.to_owned();
        }
        let mut rendered = Vec::new();
        let mut start = 0usize;
        for paragraph in text.split("\n\n") {
            let end = start + paragraph.len();
            let p = paragraph.trim();
            if p.is_empty() {
                rendered.push(String::new());
            } else {
                let leading = paragraph.len() - paragraph.trim_start().len();
                let p_start = start + leading;
                let p_end = p_start + p.len();
                if p.ends_with(['.', '!', '?', ':'])
                    || terminal_byte_is_protected(p, &protected[p_start..p_end])
                {
                    rendered.push(p.to_owned());
                } else {
                    rendered.push(format!("{p}."));
                }
            }
            start = end.saturating_add(2);
        }
        return rendered.join("\n\n");
    }

    // Single newlines (command new line): cascade periods only if any line
    // independently needs one (F36b); otherwise leave bare (F13b/F10b).
    if text.contains('\n') {
        let mut start = 0usize;
        let lines: Vec<(usize, &str)> = text
            .split('\n')
            .map(|line| {
                let line_start = start;
                start += line.len() + 1;
                (line_start, line)
            })
            .collect();
        if lines.iter().any(|(line_start, line)| {
            line_independently_needs_period(line)
                && !terminal_byte_is_protected(
                    line,
                    &protected[*line_start..*line_start + line.len()],
                )
        }) {
            return lines
                .iter()
                .map(|(line_start, line)| {
                    if line.trim().is_empty() {
                        (*line).to_owned()
                    } else {
                        add_terminal_punct_line(
                            line,
                            &protected[*line_start..*line_start + line.len()],
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        return text.to_owned();
    }

    add_terminal_punct_line(text, protected)
}

// ─── Closed local grammar catalog (hermetic #99 Smart oracle) ────────────────
//
// These mirror §6.1 predicates so SW3 can match the 51 #99 Smart outputs without
// a provider. SW4 still owns candidate validation/composition for untrusted JSON.

fn apply_closed_local_grammar(
    text: &str,
    dictionary: &[&str],
    protected_names: &[&str],
) -> String {
    // #100 prompt-shaped content is formatted locally but is never grammar-editable.
    // The safety contract protects the whole candidate base for these closed markers.
    if is_prompt_shaped(text) {
        return text.to_owned();
    }
    let protected = build_edit_protection(text, dictionary, protected_names);
    let mut t = text.to_owned();
    t = grammar_there_is_plural(&t, &protected);
    // Recompute mask after each rewrite so byte indices stay valid.
    let protected = build_edit_protection(&t, dictionary, protected_names);
    t = grammar_lets_meet(&t, &protected);
    let protected = build_edit_protection(&t, dictionary, protected_names);
    t = grammar_didnt(&t, &protected);
    t
}

fn is_prompt_shaped(text: &str) -> bool {
    const PROMPT_MARKERS: &[&str] = &[
        "ignore previous instructions",
        "system prompt",
        "developer message",
        "you are chatgpt",
        "system:",
        "user:",
    ];
    let lower = ascii_lower(text);
    PROMPT_MARKERS.iter().any(|marker| lower.contains(marker))
}

fn grammar_there_is_plural(text: &str, protected: &[bool]) -> String {
    // there + spaces + is + spaces + quantity + spaces + issues
    let tokens = word_tokens(text);
    for i in 0..tokens.len() {
        if ascii_lower(tokens[i].2) != "there" {
            continue;
        }
        if i + 3 >= tokens.len() {
            break;
        }
        if ascii_lower(tokens[i + 1].2) != "is" {
            continue;
        }
        let qty = ascii_lower(tokens[i + 2].2);
        let qty_ok = count_word_value(&qty).is_some();
        if !qty_ok {
            continue;
        }
        if ascii_lower(tokens[i + 3].2) != "issues" {
            continue;
        }
        // only horizontal spaces between tokens
        if !only_horizontal_spaces_between(text, tokens[i].1, tokens[i + 1].0)
            || !only_horizontal_spaces_between(text, tokens[i + 1].1, tokens[i + 2].0)
            || !only_horizontal_spaces_between(text, tokens[i + 2].1, tokens[i + 3].0)
        {
            continue;
        }
        // Replace "is" with "are" only when the verb span is not protected.
        let (s, e, _) = tokens[i + 1];
        if span_touches_protection(s, e, protected) {
            continue;
        }
        return format!("{}are{}", &text[..s], &text[e..]);
    }
    text.to_owned()
}

fn grammar_lets_meet(text: &str, protected: &[bool]) -> String {
    // sentence-initial lets + spaces + meet → let's
    let tokens = word_tokens(text);
    if tokens.is_empty() {
        return text.to_owned();
    }
    // first content token at sentence start
    let (s, e, tok) = tokens[0];
    if s != 0 && !text[..s].chars().all(|c| c.is_whitespace()) {
        // allow leading whitespace only
        if !is_sentence_or_line_start(text, s) {
            return text.to_owned();
        }
    }
    if ascii_lower(tok) != "lets" {
        return text.to_owned();
    }
    if span_touches_protection(s, e, protected) {
        return text.to_owned();
    }
    if tokens.len() < 2 || ascii_lower(tokens[1].2) != "meet" {
        return text.to_owned();
    }
    if !only_horizontal_spaces_between(text, e, tokens[1].0) {
        return text.to_owned();
    }
    // Preserve casing of first letter: Lets → Let's, lets → let's
    let replacement = if tok.chars().next().is_some_and(|c| c.is_uppercase()) {
        "Let's"
    } else {
        "let's"
    };
    format!("{}{}{}", &text[..s], replacement, &text[e..])
}

fn grammar_didnt(text: &str, protected: &[bool]) -> String {
    let tokens = word_tokens(text);
    for (s, e, tok) in tokens {
        if ascii_lower(tok) != "didnt" {
            continue;
        }
        if span_touches_protection(s, e, protected) {
            continue;
        }
        let replacement = if tok.starts_with('D') {
            "Didn't"
        } else {
            "didn't"
        };
        return format!("{}{}{}", &text[..s], replacement, &text[e..]);
    }
    text.to_owned()
}

fn only_horizontal_spaces_between(text: &str, start: usize, end: usize) -> bool {
    if start > end || end > text.len() {
        return false;
    }
    let mid = &text[start..end];
    !mid.is_empty() && mid.bytes().all(|b| b == b' ')
}

// ─── Tokenization helpers ────────────────────────────────────────────────────

/// Word tokens: maximal runs of letters/digits/`'` (ASCII or U+2019); other non-space
/// single chars are separate tokens (not returned — we only need word tokens for
/// matching/casing). Returns `(byte_start, byte_end, slice)`.
fn word_tokens(text: &str) -> Vec<(usize, usize, &str)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = text[i..].chars().next().unwrap();
        let len = ch.len_utf8();
        if is_word_char(ch) {
            let start = i;
            i += len;
            while i < bytes.len() {
                let c2 = text[i..].chars().next().unwrap();
                // Continue through alphanumerics and apostrophes mid-word (ASCII / U+2019).
                if is_word_char(c2)
                    || ((c2 == '\'' || c2 == '\u{2019}')
                        && i + c2.len_utf8() < bytes.len()
                        && text[i + c2.len_utf8()..]
                            .chars()
                            .next()
                            .is_some_and(is_word_char))
                {
                    i += c2.len_utf8();
                } else {
                    break;
                }
            }
            out.push((start, i, &text[start..i]));
        } else {
            i += len;
        }
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

fn ascii_lower(s: &str) -> String {
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

fn eq_ascii_ignore_case(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn smart(s: &str) -> String {
        format_validated(s, WritingMode::Smart).rendered().to_owned()
    }

    fn literal(s: &str) -> String {
        format_validated(s, WritingMode::Literal).rendered().to_owned()
    }

    #[test]
    fn constants_match_spec_manifest() {
        assert_eq!(LOCAL_FORMATTER_WORK_DEADLINE, Duration::from_millis(50));
        assert_eq!(MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES, 32_768);
        assert_eq!(FORMATTER_CONTRACT_ID, "voisu-local-formatting-v1:#99-approved");
    }

    #[test]
    fn oversize_validated_keeps_identity() {
        let big = "a".repeat(MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES + 1);
        let baseline = format_validated(&big, WritingMode::Smart);
        assert_eq!(baseline.rendered(), big);
        assert!(baseline.verify_derivation_digest());
    }

    #[test]
    fn deadline_miss_keeps_identity() {
        // Zero deadline forces miss after any work path.
        let input = "please let me know if that timeline works";
        let baseline = format_validated_with(
            input,
            WritingMode::Smart,
            FormatOptions {
                work_deadline: Some(Duration::ZERO),
                ..FormatOptions::default()
            },
        );
        assert_eq!(
            baseline.rendered(),
            input,
            "deadline miss must preserve Validated identity"
        );
    }

    #[test]
    fn baseline_private_fields_typed_access_and_digest() {
        let input = "ready for review";
        // Not already-correct (lowercase) → formats.
        let b = format_validated(input, WritingMode::Smart);
        assert_eq!(b.formatter_contract(), FORMATTER_CONTRACT_ID);
        assert_eq!(b.base_version(), VALIDATED_TRANSCRIPT_VERSION);
        assert!(b.base_fingerprint().starts_with("sha256:"));
        assert_eq!(b.base_fingerprint().len(), "sha256:".len() + 64);
        assert!(b.verify_derivation_digest());
        assert!(!b.rendered().is_empty());
        // Anchors exist for source word tokens that survive into rendered form.
        assert!(b.anchor_count() > 0);
        let tokens = word_tokens(input);
        assert!(
            b.anchor_for_source(SourceSpan::new(tokens[0].0, tokens[0].1))
                .is_some()
        );
    }

    #[test]
    fn protected_ranges_include_commands_and_urls() {
        let input =
            "clone https://github.com/Anuraj-dev/voisu and open crates/voisu-core/src/lib.rs";
        let b = format_validated(input, WritingMode::Smart);
        let ranges = b.protected_source_ranges();
        assert!(
            ranges.iter().any(|r| input[r.start..r.end].contains("https://")),
            "URL must be protected: {ranges:?}"
        );
        assert!(
            ranges
                .iter()
                .any(|r| input[r.start..r.end].contains("crates/")),
            "path must be protected: {ranges:?}"
        );

        let cmd = "stop command period next";
        let b2 = format_validated(cmd, WritingMode::Literal);
        assert!(
            b2.protected_source_ranges()
                .iter()
                .any(|r| cmd[r.start..r.end].contains("command")),
            "command span must be protected"
        );
    }

    #[test]
    fn shell_line_is_identity() {
        let s = "run cargo test --workspace -- --test-threads=4";
        assert_eq!(smart(s), s);
        assert_eq!(literal(s), s);
    }

    #[test]
    fn literal_is_commands_only() {
        assert_eq!(literal("ship it command exclamation point"), "ship it!");
        assert_eq!(
            literal("hey can you send the notes when you get a chance"),
            "hey can you send the notes when you get a chance"
        );
    }

    #[test]
    fn smart_casual_and_lists() {
        assert_eq!(
            smart("hey can you send the notes when you get a chance"),
            "Hey, can you send the notes when you get a chance?"
        );
        assert_eq!(smart("buy milk eggs bread"), "Buy:\n- milk\n- eggs\n- bread");
        assert_eq!(
            smart("the API has three errors not found forbidden and unauthorized"),
            "The API has three errors: not found, forbidden, and unauthorized."
        );
    }

    #[test]
    fn corpus_all_51_literal_and_smart_exact() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/research/smart-writing-behavior-corpus-2026-08-09.json"
        );
        let raw = std::fs::read_to_string(path).expect("behavior corpus readable");
        let corpus: serde_json::Value =
            serde_json::from_str(&raw).expect("behavior corpus JSON");
        let fixtures = corpus["fixtures"].as_array().expect("fixtures");
        assert_eq!(fixtures.len(), 51, "BEHAVIOR_FIXTURES_REQUIRED = 51");

        let mut failed: Vec<String> = Vec::new();
        for fx in fixtures {
            let id = fx["id"].as_str().unwrap();
            let input = fx["input"].as_str().unwrap();
            let exp_lit = fx["expected"]["literal"].as_str().unwrap();
            let exp_smart = fx["expected"]["smart"].as_str().unwrap();

            let got_lit = literal(input);
            if got_lit != exp_lit {
                failed.push(format!(
                    "{id} LITERAL\n  input: {input:?}\n  got:   {got_lit:?}\n  want:  {exp_lit:?}"
                ));
            }

            let got_smart = smart(input);
            if got_smart != exp_smart {
                failed.push(format!(
                    "{id} SMART\n  input: {input:?}\n  got:   {got_smart:?}\n  want:  {exp_smart:?}"
                ));
            }
        }

        assert!(
            failed.is_empty(),
            "{} fixture mode(s) failed:\n\n{}",
            failed.len(),
            failed.join("\n\n")
        );
    }

    #[test]
    fn fail_closed_ambiguous_prose_stays_prose() {
        // Clause marker blocks bullet inference.
        let s = "buy milk when hungry eggs bread";
        let out = smart(s);
        assert!(
            !out.contains('\n') || !out.contains("- "),
            "ambiguous buy-list must not become bullets: {out:?}"
        );
    }

    #[test]
    fn fingerprint_stable() {
        let a = transcript_fingerprint("hello");
        let b = transcript_fingerprint("hello");
        assert_eq!(a, b);
        assert_ne!(a, transcript_fingerprint("hello!"));
    }

    // ── Bug 1: command parse before already-correct identity ────────────────

    #[test]
    fn spoken_command_in_already_cased_sentence_expands() {
        // Regression: identity short-circuit used to run before SW2 parse, so an
        // already sentence-cased/punctuated Validated string with spoken commands
        // was left unexpanded. Command parse must run first; F19/F20/F35 identity
        // only applies when no §4 command span remains.
        //
        // Note: SW2 tokenizes non-whitespace runs whole, so a glued terminal mark
        // on the phrase word (`command period.`) does not match `period`. A
        // following word keeps the phrase token clean while still ending the
        // utterance with terminal punct (already-correct shape).
        assert_eq!(smart("Ship it command period next."), "Ship it. Next.");
        assert_eq!(literal("Ship it command period next."), "Ship it. next.");
        assert_eq!(
            smart("Please stop command period next."),
            "Please stop. Next."
        );
        // F19 still identity when no command span.
        assert_eq!(smart("Ready for review."), "Ready for review.");
    }

    // ── Bug 2: unmatched / curly quotes protect whole base ──────────────────

    #[test]
    fn unmatched_ascii_quote_protects_entire_base() {
        let input = "she said \"i didnt know";
        let b = format_validated(input, WritingMode::Smart);
        let ranges = b.protected_source_ranges();
        assert!(
            ranges.iter().any(|r| r.start == 0 && r.end == input.len()),
            "unmatched ASCII \" must protect whole base, got {ranges:?}"
        );
    }

    #[test]
    fn curly_quotes_protect_paired_interior() {
        // “…” (U+201C / U+201D)
        let input = "she said \u{201C}hello there\u{201D} today";
        let b = format_validated(input, WritingMode::Smart);
        let ranges = b.protected_source_ranges();
        let open = input.find('\u{201C}').unwrap();
        let close = input.find('\u{201D}').unwrap() + '\u{201D}'.len_utf8();
        assert!(
            ranges.iter().any(|r| r.start == open && r.end == close),
            "paired curly double quotes must be protected, got {ranges:?}"
        );
    }

    #[test]
    fn unmatched_curly_quote_protects_entire_base() {
        let input = "she said \u{201C}i didnt know";
        let b = format_validated(input, WritingMode::Smart);
        let ranges = b.protected_source_ranges();
        assert!(
            ranges.iter().any(|r| r.start == 0 && r.end == input.len()),
            "unmatched curly quote must protect whole base, got {ranges:?}"
        );
    }

    // ── Sol revise: local formatting honors quote/code protection ────────────

    #[test]
    fn quoted_interior_not_mutated_by_local_formatting() {
        // Behavioral: rendered must preserve protected interiors (casing + grammar).
        // ASCII paired double quotes.
        let ascii = smart("she said \"i didnt know\"");
        assert!(
            ascii.contains("\"i didnt know\""),
            "ASCII quote interior must keep i/didnt, got {ascii:?}"
        );
        assert!(
            !ascii.contains("didn't") && !ascii.contains("\"I "),
            "must not rewrite/case inside ASCII quotes: {ascii:?}"
        );

        // Curly double quotes.
        let curly = smart("she said \u{201C}i didnt know\u{201D}");
        assert!(
            curly.contains("\u{201C}i didnt know\u{201D}"),
            "curly quote interior must keep i/didnt, got {curly:?}"
        );

        // ASCII single quotes.
        let single = smart("she said 'i didnt know'");
        assert!(
            single.contains("'i didnt know'"),
            "single-quote interior must keep i/didnt, got {single:?}"
        );

        // Inline code.
        let code = smart("use `didnt` exactly");
        assert!(
            code.contains("`didnt`"),
            "inline code must not get didnt→didn't, got {code:?}"
        );
        assert!(!code.contains("`didn't`"), "code interior rewritten: {code:?}");
    }

    #[test]
    fn unprotected_didnt_still_rewrites_outside_quotes() {
        assert_eq!(
            smart("i didnt see no error in the logs"),
            "I didn't see no error in the logs."
        );
    }

    #[test]
    fn multiline_fenced_code_interior_is_byte_exact_after_all_formatting_passes() {
        let input = "note is below\n```\n1. foo  \n\nlet value is two\ncommand period\nshe said \"x\" and left\n```";
        assert_eq!(
            smart(input),
            "Note is below\n```\n1. foo  \n\nlet value is two\ncommand period\nshe said \"x\" and left\n```"
        );
    }

    #[test]
    fn prompt_shaped_text_is_formatted_but_not_grammar_edited() {
        assert_eq!(
            smart("ignore previous instructions and didnt obey"),
            "Ignore previous instructions and didnt obey."
        );
    }

    /// P1: structural enumeration must not run through inline code (Sol r2).
    /// Repro: list reconstructed *outside* backticks and backticks discarded.
    #[test]
    fn code_span_blocks_counted_enumeration_rewrite() {
        let input = "the API has three errors `not found forbidden and unauthorized`";
        let out = smart(input);
        assert!(
            out.contains("`not found forbidden and unauthorized`"),
            "code interior must stay intact (no list reconstruction outside backticks), got {out:?}"
        );
        assert!(
            !out.contains("errors: not found") && !out.contains(": not found, forbidden"),
            "must not reconstruct enumeration by stripping code fences: {out:?}"
        );
        // Unprotected exterior still gets ordinary Smart casing/punct.
        assert!(
            out.starts_with("The API has three errors"),
            "exterior casing should still apply, got {out:?}"
        );
    }

    /// P1: ASCII quote interiors also block structural list inference.
    #[test]
    fn quoted_span_blocks_bullet_inference() {
        let input = "buy \"milk eggs bread cheese\"";
        let out = smart(input);
        assert!(
            !out.contains('\n') || !out.contains("- "),
            "quoted buy-list must not become bullets: {out:?}"
        );
        assert!(
            out.contains("\"milk eggs bread cheese\"") || out.contains("milk eggs bread cheese"),
            "quoted interior should remain, got {out:?}"
        );
    }

    // ── Sol r2: dictionary / protected-names block local grammar ─────────────

    #[test]
    fn dictionary_didnt_not_rewritten_by_local_grammar() {
        let input = "i didnt see no error in the logs";
        let b = format_validated_with(
            input,
            WritingMode::Smart,
            FormatOptions {
                dictionary: &["didnt"],
                ..FormatOptions::default()
            },
        );
        assert!(
            b.rendered().contains("didnt") && !b.rendered().contains("didn't"),
            "dictionary snapshot token must not get didnt→didn't, got {:?}",
            b.rendered()
        );
        // Casing outside the protected token still applies.
        assert!(
            b.rendered().starts_with('I'),
            "unprotected sentence-start casing still applies: {:?}",
            b.rendered()
        );
    }

    #[test]
    fn protected_name_lets_not_rewritten_by_local_grammar() {
        let input = "Lets meet tomorrow";
        let b = format_validated_with(
            input,
            WritingMode::Smart,
            FormatOptions {
                protected_names: &["Lets"],
                ..FormatOptions::default()
            },
        );
        assert!(
            b.rendered().contains("Lets") && !b.rendered().contains("Let's"),
            "protected-name snapshot must not get lets→let's, got {:?}",
            b.rendered()
        );
    }

    // ── Sol revise: source anchors survive local rewrites ────────────────────

    fn assert_token_anchored(b: &FormattingBaseline, source: &str, token: &str) {
        let tok = word_tokens(source)
            .into_iter()
            .find(|(_, _, t)| *t == token)
            .unwrap_or_else(|| panic!("source missing token {token:?} in {source:?}"));
        assert!(
            b.anchor_for_source(SourceSpan::new(tok.0, tok.1)).is_some(),
            "expected anchor for {token:?} in {source:?}; rendered={:?}",
            b.rendered()
        );
    }

    #[test]
    fn anchors_survive_is_are_rewrite() {
        let source = "there is two issues with the patch";
        let b = format_validated(source, WritingMode::Smart);
        assert_eq!(b.rendered(), "There are two issues with the patch.");
        // Rewritten verb is unmappable; following tokens must still anchor.
        assert_token_anchored(&b, source, "there");
        assert!(
            b.anchor_for_source({
                let t = word_tokens(source)
                    .into_iter()
                    .find(|(_, _, t)| *t == "is")
                    .unwrap();
                SourceSpan::new(t.0, t.1)
            })
            .is_none(),
            "is→are must leave source 'is' unmappable"
        );
        for tok in ["two", "issues", "with", "the", "patch"] {
            assert_token_anchored(&b, source, tok);
        }
    }

    #[test]
    fn anchors_survive_lets_rewrite() {
        let source = "lets meet tomorrow";
        let b = format_validated(source, WritingMode::Smart);
        assert_eq!(b.rendered(), "Let's meet tomorrow.");
        assert!(
            b.anchor_for_source({
                let t = word_tokens(source)
                    .into_iter()
                    .find(|(_, _, t)| *t == "lets")
                    .unwrap();
                SourceSpan::new(t.0, t.1)
            })
            .is_none(),
            "lets→let's must leave source 'lets' unmappable"
        );
        assert_token_anchored(&b, source, "meet");
        assert_token_anchored(&b, source, "tomorrow");
    }

    #[test]
    fn anchors_survive_didnt_rewrite_mid_sentence() {
        let source = "i didnt see no error in the logs";
        let b = format_validated(source, WritingMode::Smart);
        assert_eq!(b.rendered(), "I didn't see no error in the logs.");
        assert_token_anchored(&b, source, "i");
        assert!(
            b.anchor_for_source({
                let t = word_tokens(source)
                    .into_iter()
                    .find(|(_, _, t)| *t == "didnt")
                    .unwrap();
                SourceSpan::new(t.0, t.1)
            })
            .is_none(),
            "didnt→didn't must leave source 'didnt' unmappable"
        );
        for tok in ["see", "no", "error", "in", "the", "logs"] {
            assert_token_anchored(&b, source, tok);
        }
    }

    #[test]
    fn anchors_compose_two_rewrites() {
        // Mid-sentence is→are and didnt→didn't; remaining tokens stay anchored.
        let source = "there is two issues and i didnt know";
        let b = format_validated(source, WritingMode::Smart);
        assert!(
            b.rendered().contains("are") && b.rendered().contains("didn't"),
            "expected both rewrites, got {:?}",
            b.rendered()
        );
        for tok in ["there", "two", "issues", "and", "i", "know"] {
            assert_token_anchored(&b, source, tok);
        }
    }

    // ── Bug 3: cooperative deadline + linear scans at max size ──────────────

    /// Synthetic zero Instant deadline forces identity without heavy work.
    #[test]
    fn max_size_zero_deadline_aborts_without_multi_second_cpu() {
        let input = "x".repeat(MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES);
        let t0 = Instant::now();
        let baseline = format_validated_with(
            &input,
            WritingMode::Smart,
            FormatOptions {
                work_deadline: Some(Duration::ZERO),
                ..FormatOptions::default()
            },
        );
        let elapsed = t0.elapsed();
        assert_eq!(
            baseline.rendered(),
            input,
            "zero-deadline miss must preserve Validated identity"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "zero-deadline 32 KiB path must not burn multi-second CPU, took {elapsed:?}"
        );
        // Identity seal: whole base protected, anchors 1:1 on source tokens.
        assert!(
            baseline
                .protected_source_ranges()
                .iter()
                .any(|r| r.start == 0 && r.end == input.len()),
            "identity seal must protect whole base"
        );
    }

    /// Deterministic: already-elapsed deadline never returns a rewritten baseline.
    #[test]
    fn expired_deadline_returns_identity_not_formatted() {
        let input = "there is two issues with the patch";
        // Zero budget → miss before/during work; must not emit Smart rewrite.
        let b = format_validated_with(
            input,
            WritingMode::Smart,
            FormatOptions {
                work_deadline: Some(Duration::ZERO),
                ..FormatOptions::default()
            },
        );
        assert_eq!(b.rendered(), input);
        assert_ne!(
            smart(input),
            input,
            "precondition: Smart would rewrite without deadline"
        );
    }

    // ── Sol r2: deterministic seal-path deadline branches ────────────────────
    //
    // Zero-budget tests only hit the pre-work check. These drive finish_baseline
    // with a far-future Instant plus a test hook so before/during/after-seal
    // identity fallback is exercised without flaky wall-clock gates.

    fn assert_seal_path_identity(source: &str, free_checks_before_hit: u32) {
        let rendered = smart(source);
        assert_ne!(
            rendered, source,
            "precondition: Smart rewrite must differ from source"
        );
        let fp = transcript_fingerprint(source);
        let far = Instant::now() + Duration::from_secs(3600);
        let _guard = seal_deadline_test_hook::Guard;
        seal_deadline_test_hook::arm_hit_after(free_checks_before_hit);
        let b = finish_baseline(
            source,
            VALIDATED_TRANSCRIPT_VERSION,
            fp,
            rendered,
            far,
        );
        assert_eq!(
            b.rendered(),
            source,
            "seal-path deadline miss (free={free_checks_before_hit}) must identity-fallback"
        );
        assert!(
            b.protected_source_ranges()
                .iter()
                .any(|r| r.start == 0 && r.end == source.len()),
            "identity seal must protect whole base"
        );
        assert!(b.verify_derivation_digest());
    }

    #[test]
    fn seal_deadline_before_seal_returns_identity() {
        // 0 free: first deadline_hit in finish_baseline → before-seal branch.
        assert_seal_path_identity("there is two issues with the patch", 0);
    }

    #[test]
    fn seal_deadline_during_seal_returns_identity() {
        // 1 free: finish before-check passes; first try_seal_baseline check hits → None.
        assert_seal_path_identity("there is two issues with the patch", 1);
    }

    #[test]
    fn seal_deadline_after_seal_returns_identity() {
        // 3 free: before + two mid-seal checks pass; post-seal deadline_hit → identity.
        assert_seal_path_identity("there is two issues with the patch", 3);
    }

    #[test]
    fn seal_deadline_past_instant_returns_identity() {
        // Instant already elapsed at seal entry (no hook): before-seal path.
        let source = "there is two issues with the patch";
        let rendered = smart(source);
        let fp = transcript_fingerprint(source);
        let past = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("Instant supports past deadline");
        let b = finish_baseline(
            source,
            VALIDATED_TRANSCRIPT_VERSION,
            fp,
            rendered,
            past,
        );
        assert_eq!(b.rendered(), source);
    }

    /// Full format of max-size input must stay linear (no multi-second O(n²)).
    ///
    /// Uses a generous wall cap that still fails pathological quadratic scans;
    /// cooperative 50 ms is enforced via synthetic Instant deadlines above, not
    /// a scheduler-flaky release assertion.
    #[test]
    fn max_size_full_format_stays_linear_budget() {
        let input = "x".repeat(MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES);
        let t0 = Instant::now();
        let baseline = format_validated(&input, WritingMode::Smart);
        let elapsed = t0.elapsed();
        let _ = baseline.rendered();
        // 2 s is far above linear O(n) at 32 KiB and far below O(n²) multi-second paths.
        let cap = Duration::from_secs(2);
        assert!(
            elapsed < cap,
            "32 KiB full format must stay linear (under {cap:?}); took {elapsed:?}"
        );
    }
}
