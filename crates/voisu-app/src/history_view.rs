//! Human-first rendering of `voisu history` for the CLI.
//!
//! The daemon returns the retained diagnostic history as structured JSON
//! (newest first). Raja reads this constantly to judge dictation latency, so the
//! default `voisu history` view is a compact, scannable block per Recording that
//! foregrounds the tail latency and each Provider's outcome — not a raw JSON
//! dump. `voisu history --json` keeps the byte-for-byte JSON escape hatch.
//!
//! The rendering is a pure function over the parsed JSON (`serde_json::Value`)
//! plus a caller-supplied clock and style. Keeping it pure — JSON in, String out
//! — makes tail computation, missing timing fields, disabled Providers, failure
//! records, truncation, and the non-interactive path all directly testable
//! without a daemon. The daemon IPC protocol and stored records are unchanged;
//! this is presentation only.
//!
//! Every externally sourced string (delivered Transcript, Provider diagnostics,
//! decision reasons) originates from a network STT provider and is therefore
//! untrusted. All such text is routed through [`truncate_inline`], which strips
//! terminal control bytes (ESC/BEL/backspace/DEL and other C0/C1 controls) so a
//! hostile transcript cannot smuggle CSI screen-clears or OSC clipboard-hijack
//! sequences into the user's terminal — even piped with color off.

use serde_json::Value;

/// Default number of Recordings shown per page.
pub const DEFAULT_PAGE_SIZE: usize = 20;

/// How wide the delivered Transcript is allowed to be before it is truncated for
/// terminal display. The stored text is already clamped far larger; this is a
/// scannability bound, not a storage bound.
const DEFAULT_TRANSCRIPT_WIDTH: usize = 72;

/// Presentation knobs for a render: the reference clock for relative times,
/// whether to emit ANSI color (gate this on a stdout TTY), and the Transcript
/// display width.
#[derive(Clone, Copy, Debug)]
pub struct RenderStyle {
    /// The "now" used to compute human-relative times, in Unix milliseconds.
    pub now_ms: u64,
    /// Emit ANSI color escapes. Callers must only enable this for a real TTY.
    pub color: bool,
    /// Maximum displayed Transcript width in characters before truncation.
    pub transcript_width: usize,
}

impl RenderStyle {
    /// A plain, color-free style anchored at `now_ms`. Used by tests and by the
    /// non-TTY path.
    pub fn plain(now_ms: u64) -> Self {
        Self {
            now_ms,
            color: false,
            transcript_width: DEFAULT_TRANSCRIPT_WIDTH,
        }
    }
}

/// One rendered page of history: the text to print, how many Recordings it
/// covered, and how many older Recordings remain after it.
#[derive(Clone, Debug)]
pub struct Page {
    /// The rendered Recordings for this page. Always newline-terminated.
    pub body: String,
    /// How many Recordings this page rendered.
    pub shown: usize,
    /// How many Recordings remain after this page.
    pub remaining: usize,
}

/// Renders one page of `records[start .. start + page_size]`.
///
/// `records` is the `history` array exactly as the daemon returned it (newest
/// first). Any non-array value renders as an empty history. Absent fields are
/// tolerated: old records may lack timing fields, and those simply render as
/// `—`.
pub fn render_page(records: &Value, start: usize, page_size: usize, style: &RenderStyle) -> Page {
    let empty = Vec::new();
    let all = records.as_array().unwrap_or(&empty);
    if all.is_empty() {
        return Page {
            body: "No Recordings in local history.\n".to_owned(),
            shown: 0,
            remaining: 0,
        };
    }
    let start = start.min(all.len());
    let end = start.saturating_add(page_size).min(all.len());
    let slice = &all[start..end];
    if slice.is_empty() {
        return Page {
            body: String::new(),
            shown: 0,
            remaining: all.len().saturating_sub(start),
        };
    }
    let mut body = String::new();
    for (offset, record) in slice.iter().enumerate() {
        let index = start + offset + 1;
        body.push_str(&render_record(index, record, style));
        body.push('\n');
    }
    Page {
        body,
        shown: slice.len(),
        remaining: all.len().saturating_sub(end),
    }
}

/// Renders the first page for a non-interactive (piped / scripted) reader:
/// exactly `page_size` Recordings with no blocking prompt. If older Recordings
/// remain, a single non-blocking footer line notes how many and points at the
/// `--json` escape hatch. Never waits on stdin.
pub fn render_history_noninteractive(records: &Value, page_size: usize, style: &RenderStyle) -> String {
    let page = render_page(records, 0, page_size, style);
    let mut out = page.body;
    if page.remaining > 0 {
        out.push_str(&format!(
            "\n{} older {} not shown — run `voisu history --json` for the full history.\n",
            page.remaining,
            recordings_word(page.remaining),
        ));
    }
    out
}

/// The interactive pagination prompt shown after a page when older Recordings
/// remain and stdin/stdout are a TTY.
pub fn pagination_prompt(remaining: usize, page_size: usize) -> String {
    let next = remaining.min(page_size);
    format!(
        "{} older {} — press Enter for {} more, q to quit: ",
        remaining,
        recordings_word(remaining),
        next,
    )
}

fn recordings_word(count: usize) -> &'static str {
    if count == 1 {
        "Recording"
    } else {
        "Recordings"
    }
}

fn render_record(index: usize, record: &Value, style: &RenderStyle) -> String {
    let mut lines = String::new();

    let recorded_at = u64_field(record, "recorded_at_unix_ms");
    let when = recorded_at
        .map(|then| relative_time(style.now_ms, then))
        .unwrap_or_else(|| "unknown time".to_owned());

    let capture_finalized = u64_field(record, "capture_finalized_ms");
    let release = u64_field(record, "release_to_text_ms");
    // Tail = release_to_text_ms − capture_finalized_ms. A reversed pair
    // (release earlier than capture) is an invalid record; `checked_sub`
    // renders it as `—` rather than fabricating a plausible "tail 0ms".
    let tail = match (capture_finalized, release) {
        (Some(capture), Some(release)) => release.checked_sub(capture),
        _ => None,
    };

    // Header: recency and the latency that matters.
    let header = format!(
        "{} {}  ·  tail {}  ·  release {}",
        style.paint(&format!("{index}."), Ansi::Bold),
        style.paint(&when, Ansi::Dim),
        style.paint(&millis_or_dash(tail), Ansi::CyanBold),
        style.paint(&millis_or_dash(release), Ansi::Dim),
    );
    lines.push_str(&header);
    if let Some(tag) = delivery_tag(record) {
        lines.push_str("  ");
        lines.push_str(&style.paint(&tag, Ansi::Yellow));
    }
    lines.push('\n');

    // Selection + delivered Transcript, or the failure status with a one-line
    // reason.
    let final_transcript = str_field(record, "final_transcript");
    match final_transcript {
        Some(text) if !text.is_empty() => {
            let selection = selection_label(str_field(record, "selection"));
            let shown = truncate_inline(text, style.transcript_width);
            lines.push_str(&format!(
                "   {}: {}\n",
                style.paint(selection, Ansi::CyanBold),
                style.paint(&format!("\"{shown}\""), Ansi::Reset),
            ));
        }
        _ => {
            let reason = failure_reason(record);
            let status = match reason {
                Some(reason) => format!("no Transcript delivered — {reason}"),
                None => "no Transcript delivered".to_owned(),
            };
            lines.push_str(&format!("   {}\n", style.paint(&status, Ansi::Red)));
        }
    }

    // Per-Provider outcome and timing.
    lines.push_str("   ");
    lines.push_str(&render_providers(record, style));
    lines.push('\n');

    lines
}

/// Renders each Provider referenced by this Recording with its outcome and
/// timing, joined into one scannable line.
fn render_providers(record: &Value, style: &RenderStyle) -> String {
    let timings = record.get("provider_timings_ms").and_then(Value::as_array);
    let failures = record.get("provider_failures").and_then(Value::as_array);
    let sources = record.get("source_transcripts").and_then(Value::as_array);

    // Collect every Provider mentioned anywhere, in a stable order.
    let mut providers: Vec<String> = Vec::new();
    let note = |list: Option<&Vec<Value>>, providers: &mut Vec<String>| {
        if let Some(list) = list {
            for entry in list {
                if let Some(name) = entry.get("provider").and_then(Value::as_str) {
                    if !providers.iter().any(|seen| seen == name) {
                        providers.push(name.to_owned());
                    }
                }
            }
        }
    };
    note(timings, &mut providers);
    note(failures, &mut providers);
    note(sources, &mut providers);
    providers.sort();

    if providers.is_empty() {
        return style.paint("no Provider results", Ansi::Dim);
    }

    let rendered: Vec<String> = providers
        .iter()
        .map(|provider| render_one_provider(provider, timings, failures, sources, style))
        .collect();
    rendered.join("  ·  ")
}

fn render_one_provider(
    provider: &str,
    timings: Option<&Vec<Value>>,
    failures: Option<&Vec<Value>>,
    sources: Option<&Vec<Value>>,
    style: &RenderStyle,
) -> String {
    let name = provider_display_name(provider);
    let completed = timings.and_then(|list| {
        list.iter()
            .find(|entry| entry.get("provider").and_then(Value::as_str) == Some(provider))
            .and_then(|entry| u64_field(entry, "completed_ms"))
    });
    let failure = failures.and_then(|list| {
        list.iter()
            .find(|entry| entry.get("provider").and_then(Value::as_str) == Some(provider))
    });
    let has_source = sources
        .map(|list| {
            list.iter()
                .any(|entry| entry.get("provider").and_then(Value::as_str) == Some(provider))
        })
        .unwrap_or(false);

    if let Some(completed) = completed {
        return format!(
            "{} {}",
            style.paint(&name, Ansi::Bold),
            style.paint(&format!("ok {completed}ms"), Ansi::Green),
        );
    }
    if let Some(failure) = failure {
        let stage = failure.get("stage").and_then(Value::as_str).unwrap_or("");
        let diagnostic = failure.get("diagnostic").and_then(Value::as_str).unwrap_or("");
        if stage == "not_started" {
            // A disabled Provider is a deliberate configuration, not a fault.
            if diagnostic.contains("disabled") {
                return format!(
                    "{} {}",
                    style.paint(&name, Ansi::Bold),
                    style.paint("disabled", Ansi::Yellow),
                );
            }
            return format!(
                "{} {}",
                style.paint(&name, Ansi::Bold),
                style.paint("not started", Ansi::Yellow),
            );
        }
        let mut outcome = format!("failed ({})", stage_human(stage));
        if !diagnostic.is_empty() {
            outcome.push_str(": ");
            outcome.push_str(&truncate_inline(diagnostic, 48));
        }
        return format!(
            "{} {}",
            style.paint(&name, Ansi::Bold),
            style.paint(&outcome, Ansi::Red),
        );
    }
    if has_source {
        // Produced a Source Transcript but an older record kept no timing.
        return format!(
            "{} {}",
            style.paint(&name, Ansi::Bold),
            style.paint("ok", Ansi::Green),
        );
    }
    format!("{} {}", style.paint(&name, Ansi::Bold), style.paint("—", Ansi::Dim))
}

/// A short delivery caveat when the Transcript landed via the clipboard fallback
/// or never reached the focused application. `None` for a clean compositor
/// Delivery.
fn delivery_tag(record: &Value) -> Option<String> {
    let has_transcript = str_field(record, "final_transcript")
        .map(|text| !text.is_empty())
        .unwrap_or(false);
    if !has_transcript {
        return None;
    }
    let method = str_field(record, "delivery_method");
    let count = u64_field(record, "delivery_count").unwrap_or(0);
    match method {
        Some("clipboard_fallback") => Some("(clipboard fallback)".to_owned()),
        _ if count == 0 => Some("(not delivered)".to_owned()),
        _ => None,
    }
}

/// The single most useful one-line reason a Recording produced no Transcript.
fn failure_reason(record: &Value) -> Option<String> {
    for key in ["error", "validation_reason", "fallback_reason"] {
        if let Some(reason) = str_field(record, key) {
            if !reason.is_empty() {
                return Some(truncate_inline(reason, 80));
            }
        }
    }
    None
}

fn selection_label(selection: Option<&str>) -> &'static str {
    match selection {
        Some("near_identical_groq") => "Groq",
        Some("source_groq") => "Groq",
        Some("source_deepgram") => "Deepgram",
        Some("reconciled") => "Reconciled (merged)",
        Some("repaired") => "Repaired (merged)",
        _ => "delivered",
    }
}

fn stage_human(stage: &str) -> &str {
    match stage {
        "not_started" => "not started",
        "streaming" => "streaming",
        "completion" => "completion",
        "provider_deadline" => "missed deadline",
        "aborted" => "aborted",
        other => other,
    }
}

fn provider_display_name(provider: &str) -> String {
    match provider {
        "deepgram" => "Deepgram".to_owned(),
        "groq" => "Groq".to_owned(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

fn millis_or_dash(value: Option<u64>) -> String {
    match value {
        Some(ms) => format!("{ms}ms"),
        None => "—".to_owned(),
    }
}

/// Human-relative recency. Clamps future timestamps (clock skew) to "just now".
fn relative_time(now_ms: u64, then_ms: u64) -> String {
    let delta_ms = now_ms.saturating_sub(then_ms);
    let seconds = delta_ms / 1000;
    if seconds < 5 {
        return "just now".to_owned();
    }
    if seconds < 60 {
        return format!("{seconds}s ago");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    format!("{days}d ago")
}

/// Sanitizes an untrusted, network-sourced string for safe single-line terminal
/// display and truncates it to `width` characters.
///
/// Ordinary whitespace (including tab/newline/CR) collapses to a single space so
/// the value stays on one line. Every other control character — ESC, BEL,
/// backspace, DEL, and the rest of the C0/C1 ranges — is dropped, so a hostile
/// transcript cannot inject CSI or OSC escape sequences into the terminal.
/// Truncation is measured in characters so multibyte text is never split
/// mid-codepoint.
fn truncate_inline(text: &str, width: usize) -> String {
    let collapsed: String = {
        let mut out = String::with_capacity(text.len());
        let mut last_space = false;
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !last_space {
                    out.push(' ');
                    last_space = true;
                }
            } else if ch.is_control() {
                // Drop ESC/BEL/backspace/DEL and other C0/C1 controls entirely;
                // never let them reach the terminal.
                last_space = false;
            } else {
                out.push(ch);
                last_space = false;
            }
        }
        out.trim().to_owned()
    };
    if collapsed.chars().count() <= width {
        return collapsed;
    }
    let keep = width.saturating_sub(1);
    let mut truncated: String = collapsed.chars().take(keep).collect();
    truncated.push('…');
    truncated
}

fn str_field<'a>(record: &'a Value, key: &str) -> Option<&'a str> {
    record.get(key).and_then(Value::as_str)
}

fn u64_field(record: &Value, key: &str) -> Option<u64> {
    record.get(key).and_then(Value::as_u64)
}

#[derive(Clone, Copy)]
enum Ansi {
    Reset,
    Bold,
    Dim,
    Red,
    Green,
    Yellow,
    CyanBold,
}

impl RenderStyle {
    fn paint(&self, text: &str, ansi: Ansi) -> String {
        if !self.color {
            return text.to_owned();
        }
        let code = match ansi {
            Ansi::Reset => return text.to_owned(),
            Ansi::Bold => "1",
            Ansi::Dim => "2",
            Ansi::Red => "31",
            Ansi::Green => "32",
            Ansi::Yellow => "33",
            Ansi::CyanBold => "1;36",
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_000_000_000_000;

    fn plain() -> RenderStyle {
        RenderStyle::plain(NOW)
    }

    fn render_one(record: Value) -> String {
        render_page(&json!([record]), 0, DEFAULT_PAGE_SIZE, &plain()).body
    }

    #[test]
    fn tail_is_release_minus_capture_finalized() {
        let out = render_one(json!({
            "recorded_at_unix_ms": NOW - 120_000,
            "final_transcript": "Render the full record",
            "selection": "near_identical_groq",
            "capture_finalized_ms": 1620,
            "release_to_text_ms": 1832,
            "provider_timings_ms": [
                {"provider": "deepgram", "completed_ms": 1620},
                {"provider": "groq", "completed_ms": 1450}
            ],
            "delivery_count": 1,
            "delivery_method": "compositor_submitted"
        }));
        assert!(out.contains("tail 212ms"), "{out}");
        assert!(out.contains("release 1832ms"), "{out}");
        assert!(out.contains("2m ago"), "{out}");
        assert!(out.contains("Groq: \"Render the full record\""), "{out}");
        assert!(out.contains("Deepgram ok 1620ms"), "{out}");
        assert!(out.contains("Groq ok 1450ms"), "{out}");
    }

    #[test]
    fn missing_timing_fields_render_as_dashes() {
        // An old record that predates the tail-latency fields.
        let out = render_one(json!({
            "recorded_at_unix_ms": NOW - 3_600_000,
            "final_transcript": "legacy record",
            "selection": "source_groq",
            "source_transcripts": [{"provider": "groq", "text": "legacy record"}],
            "delivery_count": 1
        }));
        assert!(out.contains("tail —"), "{out}");
        assert!(out.contains("release —"), "{out}");
        // A Source Transcript without a stored timing still reads as ok.
        assert!(out.contains("Groq ok"), "{out}");
        assert!(!out.contains("panic"), "{out}");
    }

    #[test]
    fn disabled_deepgram_reads_as_disabled_not_failed() {
        let out = render_one(json!({
            "recorded_at_unix_ms": NOW - 1000,
            "final_transcript": "just groq",
            "selection": "source_groq",
            "capture_finalized_ms": 900,
            "release_to_text_ms": 1100,
            "provider_timings_ms": [{"provider": "groq", "completed_ms": 950}],
            "provider_failures": [
                {"provider": "deepgram", "stage": "not_started",
                 "diagnostic": "Deepgram disabled for this Recording"}
            ],
            "delivery_count": 1
        }));
        assert!(out.contains("Deepgram disabled"), "{out}");
        assert!(!out.contains("failed"), "{out}");
        assert!(out.contains("Groq ok 950ms"), "{out}");
    }

    #[test]
    fn failure_record_shows_short_reason_and_provider_failure() {
        let out = render_one(json!({
            "recorded_at_unix_ms": NOW - 300_000,
            "final_transcript": null,
            "validation_reason": "quality validation failed: transcripts diverged",
            "provider_failures": [
                {"provider": "deepgram", "stage": "completion",
                 "diagnostic": "upstream returned HTTP 503"},
                {"provider": "groq", "stage": "provider_deadline",
                 "diagnostic": "Provider Deadline elapsed"}
            ]
        }));
        assert!(out.contains("no Transcript delivered — quality validation failed"), "{out}");
        assert!(out.contains("Deepgram failed (completion): upstream returned HTTP 503"), "{out}");
        assert!(out.contains("Groq failed (missed deadline)"), "{out}");
        // No JSON blob leaked into the failure view.
        assert!(!out.contains('{'), "{out}");
    }

    #[test]
    fn top_level_error_is_used_as_the_reason() {
        let out = render_one(json!({
            "recorded_at_unix_ms": NOW - 5000,
            "final_transcript": null,
            "error": "Recording Deadline elapsed"
        }));
        assert!(out.contains("no Transcript delivered — Recording Deadline elapsed"), "{out}");
    }

    #[test]
    fn long_transcript_is_truncated_with_an_ellipsis() {
        let long = "word ".repeat(40);
        let out = render_one(json!({
            "recorded_at_unix_ms": NOW,
            "final_transcript": long,
            "selection": "reconciled",
            "capture_finalized_ms": 10,
            "release_to_text_ms": 20
        }));
        assert!(out.contains('…'), "{out}");
        assert!(out.contains("Reconciled (merged)"), "{out}");
        // The displayed transcript line stays within the width bound.
        let line = out
            .lines()
            .find(|line| line.contains("Reconciled"))
            .expect("selection line present");
        assert!(line.chars().count() <= 3 + "Reconciled (merged): ".len() + DEFAULT_TRANSCRIPT_WIDTH + 2, "{line}");
    }

    #[test]
    fn newest_first_ordering_is_preserved_with_indexes() {
        let records = json!([
            {"recorded_at_unix_ms": NOW - 1000, "final_transcript": "newest",
             "selection": "source_groq", "capture_finalized_ms": 1, "release_to_text_ms": 2},
            {"recorded_at_unix_ms": NOW - 2000, "final_transcript": "older",
             "selection": "source_groq", "capture_finalized_ms": 1, "release_to_text_ms": 2}
        ]);
        let page = render_page(&records, 0, DEFAULT_PAGE_SIZE, &plain());
        let newest = page.body.find("newest").unwrap();
        let older = page.body.find("older").unwrap();
        assert!(newest < older, "{}", page.body);
        assert!(page.body.contains("1. "), "{}", page.body);
        assert!(page.body.contains("2. "), "{}", page.body);
    }

    #[test]
    fn noninteractive_prints_the_page_without_a_prompt() {
        // 25 records, default page 20: prints 20, notes 5 remaining, never
        // emits an interactive prompt or blocks on stdin.
        let records: Vec<Value> = (0..25)
            .map(|i| {
                json!({
                    "recorded_at_unix_ms": NOW - (i as u64) * 1000,
                    "final_transcript": format!("record {i}"),
                    "selection": "source_groq",
                    "capture_finalized_ms": 1,
                    "release_to_text_ms": 3
                })
            })
            .collect();
        let out = render_history_noninteractive(&json!(records), DEFAULT_PAGE_SIZE, &plain());
        assert!(out.contains("record 0"), "{out}");
        assert!(out.contains("record 19"), "{out}");
        assert!(!out.contains("record 20"), "{out}");
        assert!(out.contains("5 older Recordings not shown"), "{out}");
        assert!(!out.contains("press Enter"), "{out}");
    }

    #[test]
    fn noninteractive_small_history_has_no_footer() {
        let records = json!([
            {"recorded_at_unix_ms": NOW, "final_transcript": "only one",
             "selection": "source_groq", "capture_finalized_ms": 1, "release_to_text_ms": 2}
        ]);
        let out = render_history_noninteractive(&records, DEFAULT_PAGE_SIZE, &plain());
        assert!(out.contains("only one"), "{out}");
        assert!(!out.contains("not shown"), "{out}");
        assert!(!out.contains("press Enter"), "{out}");
    }

    #[test]
    fn empty_history_states_it_plainly() {
        let out = render_history_noninteractive(&json!([]), DEFAULT_PAGE_SIZE, &plain());
        assert_eq!(out, "No Recordings in local history.\n");
    }

    #[test]
    fn pagination_advances_and_reports_remaining() {
        let records: Vec<Value> = (0..45)
            .map(|i| json!({"recorded_at_unix_ms": NOW - (i as u64) * 1000,
                            "final_transcript": format!("record {i}"), "selection": "source_groq"}))
            .collect();
        let value = json!(records);
        let first = render_page(&value, 0, DEFAULT_PAGE_SIZE, &plain());
        assert_eq!(first.shown, 20);
        assert_eq!(first.remaining, 25);
        let second = render_page(&value, first.shown, DEFAULT_PAGE_SIZE, &plain());
        assert_eq!(second.shown, 20);
        assert_eq!(second.remaining, 5);
        let third = render_page(&value, first.shown + second.shown, DEFAULT_PAGE_SIZE, &plain());
        assert_eq!(third.shown, 5);
        assert_eq!(third.remaining, 0);
    }

    #[test]
    fn color_is_emitted_only_when_enabled() {
        let record = json!({"recorded_at_unix_ms": NOW, "final_transcript": "hi",
                            "selection": "source_groq", "capture_finalized_ms": 1,
                            "release_to_text_ms": 2});
        let plain = render_page(&json!([record.clone()]), 0, 20, &RenderStyle::plain(NOW)).body;
        assert!(!plain.contains('\x1b'), "{plain}");
        let colored_style = RenderStyle { now_ms: NOW, color: true, transcript_width: DEFAULT_TRANSCRIPT_WIDTH };
        let colored = render_page(&json!([record]), 0, 20, &colored_style).body;
        assert!(colored.contains('\x1b'), "{colored}");
    }

    #[test]
    fn non_array_history_renders_empty() {
        let out = render_page(&json!({"not": "an array"}), 0, 20, &plain());
        assert_eq!(out.shown, 0);
        assert_eq!(out.remaining, 0);
    }

    #[test]
    fn terminal_control_sequences_never_survive_in_a_transcript() {
        // A hostile Source Transcript from a network STT provider carries a CSI
        // screen-clear and an OSC 52 clipboard-hijack payload. Rendered with
        // color off (the piped path), none of the raw control bytes may reach
        // the terminal: stripping the ESC/BEL introducers renders the sequence
        // inert (any residual ASCII like "]52;c" is then harmless plain text).
        let hostile = "hi\u{1b}[2J\u{1b}[3Jthere\u{1b}]52;c;SGVsbG8=\u{7}\u{8}\u{7f}end";
        let out = render_one(json!({
            "recorded_at_unix_ms": NOW,
            "final_transcript": hostile,
            "selection": "source_groq",
            "capture_finalized_ms": 1,
            "release_to_text_ms": 2
        }));
        assert!(!out.contains('\u{1b}'), "ESC survived: {out:?}");
        assert!(!out.contains('\u{7}'), "BEL survived: {out:?}");
        assert!(!out.contains('\u{8}'), "BS survived: {out:?}");
        assert!(!out.contains('\u{7f}'), "DEL survived: {out:?}");
        // No C0/C1 control byte survives anywhere in the rendered record.
        assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "control byte survived: {out:?}");
        // Ordinary letters are preserved.
        assert!(out.contains("there"), "{out}");
    }

    #[test]
    fn terminal_control_sequences_never_survive_in_a_diagnostic_or_reason() {
        let out = render_one(json!({
            "recorded_at_unix_ms": NOW,
            "final_transcript": null,
            "validation_reason": "bad\u{1b}]52;c;cHduZWQ=\u{7} reason",
            "provider_failures": [
                {"provider": "groq", "stage": "completion",
                 "diagnostic": "upstream\u{1b}[2J failed\u{7}"}
            ]
        }));
        assert!(!out.contains('\u{1b}'), "ESC survived in diagnostic path: {out:?}");
        assert!(!out.contains('\u{7}'), "BEL survived in diagnostic path: {out:?}");
        assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "control byte survived: {out:?}");
        assert!(out.contains("failed"), "{out}");
    }

    #[test]
    fn reversed_tail_ordering_renders_a_dash_not_a_false_zero() {
        // release_to_text_ms < capture_finalized_ms is an invalid record; it
        // must not read as a plausible "tail 0ms".
        let out = render_one(json!({
            "recorded_at_unix_ms": NOW,
            "final_transcript": "reversed timings",
            "selection": "source_groq",
            "capture_finalized_ms": 1832,
            "release_to_text_ms": 1620
        }));
        assert!(out.contains("tail —"), "{out}");
        assert!(!out.contains("tail 0ms"), "{out}");
        // The release value is still surfaced as recorded.
        assert!(out.contains("release 1620ms"), "{out}");
    }
}
