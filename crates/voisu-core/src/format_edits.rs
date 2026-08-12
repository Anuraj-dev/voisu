//! Small formatting-edit contract for Developer Prompt Rendering.
//!
//! The formatting cloud job may propose only localized edits. The host parses,
//! validates, and applies them. There is no field for a free-form polished
//! string, and applying this contract never treats model prose as Delivery
//! text. Reconciliation stays on its own prompt and schema.
//!
//! Invalid JSON, stale fingerprints, unknown kinds, overlapping ranges, or
//! unanchored `before` text reject the whole candidate.

use serde_json::{Map, Value};

use crate::{is_text_sha256_fingerprint, text_sha256_fingerprint};

/// Contract id for diagnostics and later pipeline wiring.
pub const FORMAT_EDIT_CONTRACT_ID: &str = "voisu-dpr-format-edits-v1";
/// Envelope `version` accepted by this contract.
pub const FORMAT_EDIT_CONTRACT_VERSION: &str = "1";

/// Max raw candidate JSON body.
pub const MAX_FORMAT_EDIT_RESPONSE_BYTES: usize = 65_536;
/// Max JSON nesting depth for a candidate body.
pub const MAX_FORMAT_EDIT_JSON_DEPTH: usize = 8;
/// Max JSON nodes (objects/arrays/scalars) in a candidate body.
pub const MAX_FORMAT_EDIT_JSON_NODES: usize = 4_096;
/// Max `edits[]` entries.
pub const MAX_FORMAT_EDITS: usize = 64;
/// Max UTF-8 bytes per `before` / `after` field.
pub const MAX_FORMAT_EDIT_FIELD_UTF8_BYTES: usize = 2_048;

/// Closed formatting kinds. Unsupported strings fail closed.
pub const CLOSED_FORMAT_EDIT_KINDS: &[&str] = &[
    "punctuation",
    "casing",
    "whitespace_layout",
    "filler_removal",
    "clear_backtrack_removal",
    "quote_conversion",
    "structure",
    "bounded_wording",
];

/// One closed formatting operation the host can apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatEditKind {
    Punctuation,
    Casing,
    WhitespaceLayout,
    FillerRemoval,
    ClearBacktrackRemoval,
    QuoteConversion,
    Structure,
    BoundedWording,
}

impl FormatEditKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Punctuation => "punctuation",
            Self::Casing => "casing",
            Self::WhitespaceLayout => "whitespace_layout",
            Self::FillerRemoval => "filler_removal",
            Self::ClearBacktrackRemoval => "clear_backtrack_removal",
            Self::QuoteConversion => "quote_conversion",
            Self::Structure => "structure",
            Self::BoundedWording => "bounded_wording",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "punctuation" => Some(Self::Punctuation),
            "casing" => Some(Self::Casing),
            "whitespace_layout" => Some(Self::WhitespaceLayout),
            "filler_removal" => Some(Self::FillerRemoval),
            "clear_backtrack_removal" => Some(Self::ClearBacktrackRemoval),
            "quote_conversion" => Some(Self::QuoteConversion),
            "structure" => Some(Self::Structure),
            "bounded_wording" => Some(Self::BoundedWording),
            _ => None,
        }
    }
}

/// Closed failure reasons. Any of these reject the whole candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatEditErrorCode {
    Malformed,
    Oversize,
    Stale,
    UnknownKind,
    Unsorted,
    SpanOutOfBounds,
    SpanNotCharBoundary,
    AnchorMismatch,
    Overlap,
}

impl FormatEditErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "E_MALFORMED",
            Self::Oversize => "E_OVERSIZE",
            Self::Stale => "E_STALE",
            Self::UnknownKind => "E_UNKNOWN_KIND",
            Self::Unsorted => "E_UNSORTED",
            Self::SpanOutOfBounds => "E_SPAN_OUT_OF_BOUNDS",
            Self::SpanNotCharBoundary => "E_SPAN_NOT_CHAR_BOUNDARY",
            Self::AnchorMismatch => "E_ANCHOR_MISMATCH",
            Self::Overlap => "E_OVERLAP",
        }
    }
}

/// One host-applied replacement against the validated base.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatEdit {
    pub start_utf8: usize,
    pub end_utf8: usize,
    pub before: String,
    pub after: String,
    pub kind: FormatEditKind,
}

/// Parsed formatting candidate. Apply is a separate host step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatEditCandidate {
    pub version: String,
    pub base_fingerprint: String,
    pub edits: Vec<FormatEdit>,
}

/// Host composition of a formatting candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatEditOutcome {
    pub accepted: bool,
    pub rendered: String,
    pub error: Option<FormatEditErrorCode>,
}

impl FormatEditOutcome {
    #[must_use]
    fn accepted(rendered: String) -> Self {
        Self {
            accepted: true,
            rendered,
            error: None,
        }
    }

    #[must_use]
    fn rejected(base: &str, error: FormatEditErrorCode) -> Self {
        Self {
            accepted: false,
            rendered: base.to_owned(),
            error: Some(error),
        }
    }
}

/// Parse untrusted formatting JSON. Does not apply edits and does not take
/// Delivery ownership of any model string.
#[must_use]
pub fn parse_format_edit_candidate_json(raw: &[u8]) -> Result<FormatEditCandidate, FormatEditErrorCode> {
    if raw.len() > MAX_FORMAT_EDIT_RESPONSE_BYTES {
        return Err(FormatEditErrorCode::Oversize);
    }
    let value: Value = serde_json::from_slice(raw).map_err(|_| FormatEditErrorCode::Malformed)?;
    let (depth, nodes) = json_shape(&value);
    if depth > MAX_FORMAT_EDIT_JSON_DEPTH || nodes > MAX_FORMAT_EDIT_JSON_NODES {
        return Err(FormatEditErrorCode::Oversize);
    }
    parse_candidate(&value)
}

/// Validate and apply a parsed candidate against the host-owned base.
///
/// The rendered string is always composed here. A rejected candidate returns
/// the unchanged base so the caller can fall back to the local baseline.
#[must_use]
pub fn apply_format_edits(base: &str, candidate: &FormatEditCandidate) -> FormatEditOutcome {
    if candidate.version != FORMAT_EDIT_CONTRACT_VERSION {
        return FormatEditOutcome::rejected(base, FormatEditErrorCode::Malformed);
    }
    if !is_text_sha256_fingerprint(&candidate.base_fingerprint)
        || candidate.base_fingerprint != text_sha256_fingerprint(base)
    {
        return FormatEditOutcome::rejected(base, FormatEditErrorCode::Stale);
    }
    if let Err(error) = validate_edits(base, &candidate.edits) {
        return FormatEditOutcome::rejected(base, error);
    }
    FormatEditOutcome::accepted(compose_edits(base, &candidate.edits))
}

/// Parse, validate, and apply one raw formatting body against the host base.
#[must_use]
pub fn apply_format_edit_candidate_json(base: &str, raw: &[u8]) -> FormatEditOutcome {
    match parse_format_edit_candidate_json(raw) {
        Ok(candidate) => apply_format_edits(base, &candidate),
        Err(error) => FormatEditOutcome::rejected(base, error),
    }
}

fn parse_candidate(value: &Value) -> Result<FormatEditCandidate, FormatEditErrorCode> {
    let object = value.as_object().ok_or(FormatEditErrorCode::Malformed)?;
    if !has_exact_keys(object, &["version", "base_fingerprint", "edits"]) {
        return Err(FormatEditErrorCode::Malformed);
    }
    let version = object
        .get("version")
        .and_then(Value::as_str)
        .ok_or(FormatEditErrorCode::Malformed)?;
    if version != FORMAT_EDIT_CONTRACT_VERSION {
        return Err(FormatEditErrorCode::Malformed);
    }
    let base_fingerprint = object
        .get("base_fingerprint")
        .and_then(Value::as_str)
        .ok_or(FormatEditErrorCode::Malformed)?;
    if !is_text_sha256_fingerprint(base_fingerprint) {
        return Err(FormatEditErrorCode::Malformed);
    }
    let raw_edits = object
        .get("edits")
        .and_then(Value::as_array)
        .ok_or(FormatEditErrorCode::Malformed)?;
    if raw_edits.len() > MAX_FORMAT_EDITS {
        return Err(FormatEditErrorCode::Oversize);
    }
    let mut edits = Vec::with_capacity(raw_edits.len());
    for raw in raw_edits {
        edits.push(parse_edit(raw)?);
    }
    if !edits.windows(2).all(|pair| {
        (pair[0].start_utf8, pair[0].end_utf8) <= (pair[1].start_utf8, pair[1].end_utf8)
    }) {
        return Err(FormatEditErrorCode::Unsorted);
    }
    Ok(FormatEditCandidate {
        version: version.to_owned(),
        base_fingerprint: base_fingerprint.to_owned(),
        edits,
    })
}

fn parse_edit(value: &Value) -> Result<FormatEdit, FormatEditErrorCode> {
    let object = value.as_object().ok_or(FormatEditErrorCode::Malformed)?;
    if !has_exact_keys(
        object,
        &["start_utf8", "end_utf8", "before", "after", "kind"],
    ) {
        return Err(FormatEditErrorCode::Malformed);
    }
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(FormatEditErrorCode::Malformed)?;
    let Some(kind) = FormatEditKind::parse(kind) else {
        return Err(FormatEditErrorCode::UnknownKind);
    };
    let before = object
        .get("before")
        .and_then(Value::as_str)
        .ok_or(FormatEditErrorCode::Malformed)?;
    let after = object
        .get("after")
        .and_then(Value::as_str)
        .ok_or(FormatEditErrorCode::Malformed)?;
    if before.len() > MAX_FORMAT_EDIT_FIELD_UTF8_BYTES
        || after.len() > MAX_FORMAT_EDIT_FIELD_UTF8_BYTES
    {
        return Err(FormatEditErrorCode::Oversize);
    }
    let start = object
        .get("start_utf8")
        .and_then(as_usize)
        .ok_or(FormatEditErrorCode::Malformed)?;
    let end = object
        .get("end_utf8")
        .and_then(as_usize)
        .ok_or(FormatEditErrorCode::Malformed)?;
    Ok(FormatEdit {
        start_utf8: start,
        end_utf8: end,
        before: before.to_owned(),
        after: after.to_owned(),
        kind,
    })
}

fn validate_edits(base: &str, edits: &[FormatEdit]) -> Result<(), FormatEditErrorCode> {
    for (index, edit) in edits.iter().enumerate() {
        if edit.start_utf8 > edit.end_utf8 || edit.end_utf8 > base.len() {
            return Err(FormatEditErrorCode::SpanOutOfBounds);
        }
        if !base.is_char_boundary(edit.start_utf8) || !base.is_char_boundary(edit.end_utf8) {
            return Err(FormatEditErrorCode::SpanNotCharBoundary);
        }
        if base.get(edit.start_utf8..edit.end_utf8) != Some(edit.before.as_str()) {
            return Err(FormatEditErrorCode::AnchorMismatch);
        }
        for other in &edits[index + 1..] {
            if ranges_overlap(
                (edit.start_utf8, edit.end_utf8),
                (other.start_utf8, other.end_utf8),
            ) {
                return Err(FormatEditErrorCode::Overlap);
            }
        }
    }
    Ok(())
}

fn compose_edits(base: &str, edits: &[FormatEdit]) -> String {
    let mut rendered = base.to_owned();
    let mut replacements: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|edit| (edit.start_utf8, edit.end_utf8, edit.after.as_str()))
        .collect();
    replacements.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    for (start, end, after) in replacements {
        rendered.replace_range(start..end, after);
    }
    rendered
}

fn ranges_overlap(left: (usize, usize), right: (usize, usize)) -> bool {
    left.0 < right.1 && right.0 < left.1
}

fn has_exact_keys(object: &Map<String, Value>, expected: &[&str]) -> bool {
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn as_usize(value: &Value) -> Option<usize> {
    value.as_u64().and_then(|number| usize::try_from(number).ok())
}

fn json_shape(value: &Value) -> (usize, usize) {
    match value {
        Value::Array(values) => {
            let mut depth = 1usize;
            let mut nodes = 1usize;
            for child in values {
                let (child_depth, child_nodes) = json_shape(child);
                depth = depth.max(child_depth.saturating_add(1));
                nodes = nodes.saturating_add(child_nodes);
            }
            (depth, nodes)
        }
        Value::Object(values) => {
            let mut depth = 1usize;
            let mut nodes = 1usize;
            for child in values.values() {
                let (child_depth, child_nodes) = json_shape(child);
                depth = depth.max(child_depth.saturating_add(1));
                nodes = nodes.saturating_add(child_nodes);
            }
            (depth, nodes)
        }
        _ => (0, 1),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn candidate_json(base: &str, edits: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "version": FORMAT_EDIT_CONTRACT_VERSION,
            "base_fingerprint": text_sha256_fingerprint(base),
            "edits": edits,
        }))
        .expect("candidate JSON")
    }

    fn edit(start: usize, end: usize, before: &str, after: &str, kind: &str) -> Value {
        json!({
            "start_utf8": start,
            "end_utf8": end,
            "before": before,
            "after": after,
            "kind": kind,
        })
    }

    #[test]
    fn closed_kinds_match_parser() {
        assert_eq!(CLOSED_FORMAT_EDIT_KINDS.len(), 8);
        for kind in CLOSED_FORMAT_EDIT_KINDS {
            let parsed = FormatEditKind::parse(kind).expect(kind);
            assert_eq!(parsed.as_str(), *kind);
        }
        assert!(FormatEditKind::parse("keep").is_none());
        assert!(FormatEditKind::parse("derivation").is_none());
    }

    #[test]
    fn host_applies_structure_edit_and_never_takes_model_prose() {
        let base = "goal ship the rust parser";
        let raw = candidate_json(
            base,
            json!([edit(0, 4, "goal", "Goal:\n", "structure")]),
        );
        let parsed = parse_format_edit_candidate_json(&raw).expect("parse");
        assert_eq!(parsed.version, "1");
        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].kind, FormatEditKind::Structure);

        let outcome = apply_format_edits(base, &parsed);
        assert!(outcome.accepted);
        assert_eq!(outcome.rendered, "Goal:\n ship the rust parser");
        assert!(outcome.error.is_none());

        let prose = json!({
            "version": "1",
            "base_fingerprint": text_sha256_fingerprint(base),
            "rendered": "Goal: invent a new API and ship it",
        });
        let rejected = apply_format_edit_candidate_json(base, prose.to_string().as_bytes());
        assert!(!rejected.accepted);
        assert_eq!(rejected.error, Some(FormatEditErrorCode::Malformed));
        assert_eq!(rejected.rendered, base);
    }

    #[test]
    fn applies_closed_kinds_without_overlapping() {
        let base = "um the goal is quote hello unquote";
        let um_end = 2;
        let goal_start = base.find("goal").unwrap();
        let quote_start = base.find("quote hello unquote").unwrap();
        let quote_end = quote_start + "quote hello unquote".len();
        let raw = candidate_json(
            base,
            json!([
                edit(0, um_end, "um", "", "filler_removal"),
                edit(goal_start, goal_start + 4, "goal", "Goal", "casing"),
                edit(
                    quote_start,
                    quote_end,
                    "quote hello unquote",
                    "\"hello\"",
                    "quote_conversion"
                ),
            ]),
        );
        let outcome = apply_format_edit_candidate_json(base, &raw);
        assert!(outcome.accepted, "{outcome:?}");
        assert_eq!(outcome.rendered, " the Goal is \"hello\"");
    }

    #[test]
    fn empty_edits_are_host_identity() {
        let base = "leave this wording";
        let outcome = apply_format_edit_candidate_json(base, &candidate_json(base, json!([])));
        assert!(outcome.accepted);
        assert_eq!(outcome.rendered, base);
    }

    #[test]
    fn stale_fingerprint_and_unknown_kind_reject_the_candidate() {
        let base = "goal";
        let stale = json!({
            "version": "1",
            "base_fingerprint": format!("sha256:{}", "0".repeat(64)),
            "edits": [edit(0, 4, "goal", "Goal:\n", "structure")],
        });
        let outcome = apply_format_edit_candidate_json(base, stale.to_string().as_bytes());
        assert!(!outcome.accepted);
        assert_eq!(outcome.error, Some(FormatEditErrorCode::Stale));
        assert_eq!(outcome.rendered, base);

        let unknown = apply_format_edit_candidate_json(
            base,
            &candidate_json(base, json!([edit(0, 4, "goal", "Goal", "rewrite")])),
        );
        assert_eq!(unknown.error, Some(FormatEditErrorCode::UnknownKind));
        assert_eq!(unknown.rendered, base);
    }

    #[test]
    fn overlapping_unsorted_and_unanchored_edits_fail_closed() {
        let base = "goal notes";
        let overlap = apply_format_edit_candidate_json(
            base,
            &candidate_json(
                base,
                json!([
                    edit(0, 4, "goal", "Goal", "casing"),
                    edit(2, 6, "al n", "AL N", "casing"),
                ]),
            ),
        );
        assert_eq!(overlap.error, Some(FormatEditErrorCode::Overlap));

        let unsorted = apply_format_edit_candidate_json(
            base,
            &candidate_json(
                base,
                json!([
                    edit(5, 10, "notes", "Notes", "casing"),
                    edit(0, 4, "goal", "Goal", "casing"),
                ]),
            ),
        );
        assert_eq!(unsorted.error, Some(FormatEditErrorCode::Unsorted));

        let mismatch = apply_format_edit_candidate_json(
            base,
            &candidate_json(base, json!([edit(0, 4, "GOAL", "Goal", "casing")])),
        );
        assert_eq!(mismatch.error, Some(FormatEditErrorCode::AnchorMismatch));
    }

    #[test]
    fn malformed_and_oversize_bodies_fail_closed() {
        let base = "goal";
        assert_eq!(
            apply_format_edit_candidate_json(base, b"not json").error,
            Some(FormatEditErrorCode::Malformed)
        );
        assert_eq!(
            apply_format_edit_candidate_json(base, &vec![b' '; MAX_FORMAT_EDIT_RESPONSE_BYTES + 1])
                .error,
            Some(FormatEditErrorCode::Oversize)
        );

        let missing_kind = json!({
            "version": "1",
            "base_fingerprint": text_sha256_fingerprint(base),
            "edits": [{
                "start_utf8": 0,
                "end_utf8": 4,
                "before": "goal",
                "after": "Goal",
            }],
        });
        assert_eq!(
            apply_format_edit_candidate_json(base, missing_kind.to_string().as_bytes()).error,
            Some(FormatEditErrorCode::Malformed)
        );

        let out_of_bounds = apply_format_edit_candidate_json(
            base,
            &candidate_json(base, json!([edit(0, 99, "goal", "Goal", "casing")])),
        );
        assert_eq!(out_of_bounds.error, Some(FormatEditErrorCode::SpanOutOfBounds));

        let mid_char = "gøal";
        let cut = apply_format_edit_candidate_json(
            mid_char,
            &candidate_json(mid_char, json!([edit(1, 2, "ø", "o", "bounded_wording")])),
        );
        assert_eq!(cut.error, Some(FormatEditErrorCode::SpanNotCharBoundary));
    }

    #[test]
    fn punctuation_layout_and_backtrack_kinds_compose_locally() {
        let punct = apply_format_edit_candidate_json(
            "ship it",
            &candidate_json("ship it", json!([edit(7, 7, "", ".", "punctuation")])),
        );
        assert_eq!(punct.rendered, "ship it.");

        let layout = apply_format_edit_candidate_json(
            "one two",
            &candidate_json(
                "one two",
                json!([edit(3, 4, " ", "\n", "whitespace_layout")]),
            ),
        );
        assert_eq!(layout.rendered, "one\ntwo");

        let backtrack = apply_format_edit_candidate_json(
            "send the red no the blue file",
            &candidate_json(
                "send the red no the blue file",
                json!([edit(9, 20, "red no the ", "", "clear_backtrack_removal")]),
            ),
        );
        assert_eq!(backtrack.rendered, "send the blue file");
    }
}
