//! Combined-call validator + hierarchical compose (DPR-T3 / #158, #139 v1.1.2).
//!
//! Sole accept path for model-structured organize candidates. There is
//! **intentionally no public API** that takes free-form model prose and returns
//! it as Final Transcript. Composition concatenates derivation `output_text`
//! spans only after gates pass; hard failures fall back to the local baseline.
//!
//! Behavior authority: `docs/research/developer-prompt-rendering-combined-call-prototype-2026-08-11.py`
//! (`compose_fixture` + helpers) and corpus version **1.1.2**.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use regex::Regex;
use serde::Deserialize;

use crate::local_baseline::LocalBaseline;
use crate::prompt_rendering::{
    RenderingPolicy, CLOSED_STRUCTURED_LABELS, DELIVERY_AUTO_SEND, DELIVERY_LIVE_TYPE,
    DELIVERY_REPLACE_DELIVERED, DELIVERY_STATE_UNSENT,
};
use crate::text_sha256_fingerprint;

/// Contract id for diagnostics / later pipeline wiring.
pub const COMPOSE_GATE_CONTRACT_ID: &str = "voisu-dpr-compose-gate-v1.1.2:#158";

// ─── Untrusted candidate JSON bounds (grammar_safety spirit; research thresholds) ──

/// Max raw candidate JSON body (tighter than corpus file bound; mirrors grammar).
pub const MAX_COMPOSE_CANDIDATE_BYTES: usize = 65_536;
/// Max JSON nesting depth for a candidate body.
pub const MAX_COMPOSE_JSON_DEPTH: usize = 14;
/// Max JSON nodes (objects/arrays/scalars) in a candidate body.
pub const MAX_COMPOSE_JSON_NODES: usize = 30_000;
/// Max `removals[]` entries.
pub const MAX_COMPOSE_REMOVALS: usize = 32;
/// Max `conversions[]` entries.
pub const MAX_COMPOSE_CONVERSIONS: usize = 32;
/// Max `labels[]` entries (closed catalog size).
pub const MAX_COMPOSE_LABELS: usize = 8;
/// Max `derivation[]` spans.
pub const MAX_COMPOSE_DERIVATION_SPANS: usize = 128;
/// Max UTF-8 bytes per string field on the candidate.
pub const MAX_COMPOSE_FIELD_UTF8_BYTES: usize = 2_048;

/// Closed conversion catalog (corpus v1.1.2 / #139 prototype `DEFAULT_CONVERSIONS`).
///
/// Arrow is U+2192 (`→`); ellipsis in quote cues is U+2026 (`…`).
pub const CLOSED_CONVERSIONS: &[&str] = &[
    "exclamation point→!",
    "four→4.",
    "new line→\\n",
    "new paragraph→\\n\\n",
    "one→1.",
    "period→.",
    "quote…unquote→\"…\"",
    "spoken acceptance criteria cue→Acceptance Criteria label",
    "spoken constraints cue→Constraints label",
    "spoken context cue→Context label",
    "spoken files cue→Files label",
    "spoken goal cue→Goal label",
    "spoken notes cue→Notes label",
    "spoken requirements cue→Requirements label",
    "spoken steps cue→numbered_lines",
    "three→3.",
    "two→2.",
];

// ─── Public enums / outcome ──────────────────────────────────────────────────

/// Hierarchical composition decision (#139).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompositionDecision {
    Accept,
    AcceptPreserveWords,
    AcceptNaturalLayout,
    FallbackBaseline,
}

impl CompositionDecision {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::AcceptPreserveWords => "accept_preserve_words",
            Self::AcceptNaturalLayout => "accept_natural_layout",
            Self::FallbackBaseline => "fallback_baseline",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "accept" => Some(Self::Accept),
            "accept_preserve_words" => Some(Self::AcceptPreserveWords),
            "accept_natural_layout" => Some(Self::AcceptNaturalLayout),
            "fallback_baseline" => Some(Self::FallbackBaseline),
            _ => None,
        }
    }
}

/// Why the composer did not fully accept the structured candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FallbackTrigger {
    UnsafeSemantics,
    UnverifiableSourceDerivation,
    InvalidFixedLabel,
    UncertainBacktracking,
    UncertainLayout,
    ResponseSchemaFailure,
    ProviderFailure,
    DeadlineExceeded,
}

impl FallbackTrigger {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsafeSemantics => "unsafe_semantics",
            Self::UnverifiableSourceDerivation => "unverifiable_source_derivation",
            Self::InvalidFixedLabel => "invalid_fixed_label",
            Self::UncertainBacktracking => "uncertain_backtracking",
            Self::UncertainLayout => "uncertain_layout",
            Self::ResponseSchemaFailure => "response_schema_failure",
            Self::ProviderFailure => "provider_failure",
            Self::DeadlineExceeded => "deadline_exceeded",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "unsafe_semantics" => Some(Self::UnsafeSemantics),
            "unverifiable_source_derivation" => Some(Self::UnverifiableSourceDerivation),
            "invalid_fixed_label" => Some(Self::InvalidFixedLabel),
            "uncertain_backtracking" => Some(Self::UncertainBacktracking),
            "uncertain_layout" => Some(Self::UncertainLayout),
            "response_schema_failure" => Some(Self::ResponseSchemaFailure),
            "provider_failure" => Some(Self::ProviderFailure),
            "deadline_exceeded" => Some(Self::DeadlineExceeded),
            _ => None,
        }
    }
}

/// Closed error-code set from #139 corpus.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ComposeErrorCode {
    Malformed,
    Stale,
    UnknownConversion,
    UnknownLabel,
    Unverifiable,
    Protected,
    UnsafeSemantics,
    InvalidLabel,
    UncertainBacktrack,
    UncertainLayout,
    InventedContent,
    Overlap,
    Schema,
    Provider,
    Deadline,
    Reconcile,
}

impl ComposeErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "E_MALFORMED",
            Self::Stale => "E_STALE",
            Self::UnknownConversion => "E_UNKNOWN_CONVERSION",
            Self::UnknownLabel => "E_UNKNOWN_LABEL",
            Self::Unverifiable => "E_UNVERIFIABLE",
            Self::Protected => "E_PROTECTED",
            Self::UnsafeSemantics => "E_UNSAFE_SEMANTICS",
            Self::InvalidLabel => "E_INVALID_LABEL",
            Self::UncertainBacktrack => "E_UNCERTAIN_BACKTRACK",
            Self::UncertainLayout => "E_UNCERTAIN_LAYOUT",
            Self::InventedContent => "E_INVENTED_CONTENT",
            Self::Overlap => "E_OVERLAP",
            Self::Schema => "E_SCHEMA",
            Self::Provider => "E_PROVIDER",
            Self::Deadline => "E_DEADLINE",
            Self::Reconcile => "E_RECONCILE",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "E_MALFORMED" => Some(Self::Malformed),
            "E_STALE" => Some(Self::Stale),
            "E_UNKNOWN_CONVERSION" => Some(Self::UnknownConversion),
            "E_UNKNOWN_LABEL" => Some(Self::UnknownLabel),
            "E_UNVERIFIABLE" => Some(Self::Unverifiable),
            "E_PROTECTED" => Some(Self::Protected),
            "E_UNSAFE_SEMANTICS" => Some(Self::UnsafeSemantics),
            "E_INVALID_LABEL" => Some(Self::InvalidLabel),
            "E_UNCERTAIN_BACKTRACK" => Some(Self::UncertainBacktrack),
            "E_UNCERTAIN_LAYOUT" => Some(Self::UncertainLayout),
            "E_INVENTED_CONTENT" => Some(Self::InventedContent),
            "E_OVERLAP" => Some(Self::Overlap),
            "E_SCHEMA" => Some(Self::Schema),
            "E_PROVIDER" => Some(Self::Provider),
            "E_DEADLINE" => Some(Self::Deadline),
            "E_RECONCILE" => Some(Self::Reconcile),
            _ => None,
        }
    }
}

/// Host-observed cloud attempt outcome (before compose).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloudOutcome {
    Succeeded,
    RejectedUnsafe,
    RejectedUnverifiable,
    RejectedInvalidLabel,
    SchemaFailure,
    ProviderFailure,
    DeadlineExceeded,
    Skipped,
}

impl CloudOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::RejectedUnsafe => "rejected_unsafe",
            Self::RejectedUnverifiable => "rejected_unverifiable",
            Self::RejectedInvalidLabel => "rejected_invalid_label",
            Self::SchemaFailure => "schema_failure",
            Self::ProviderFailure => "provider_failure",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Skipped => "skipped",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "succeeded" => Some(Self::Succeeded),
            "rejected_unsafe" => Some(Self::RejectedUnsafe),
            "rejected_unverifiable" => Some(Self::RejectedUnverifiable),
            "rejected_invalid_label" => Some(Self::RejectedInvalidLabel),
            "schema_failure" => Some(Self::SchemaFailure),
            "provider_failure" => Some(Self::ProviderFailure),
            "deadline_exceeded" => Some(Self::DeadlineExceeded),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

/// Delivery flags always attached to a Final Transcript handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeliveryFlags {
    pub state: &'static str,
    pub auto_send: bool,
    pub live_type: bool,
    pub replace_delivered: bool,
}

impl DeliveryFlags {
    #[must_use]
    pub const fn dpr_default() -> Self {
        Self {
            state: DELIVERY_STATE_UNSENT,
            auto_send: DELIVERY_AUTO_SEND,
            live_type: DELIVERY_LIVE_TYPE,
            replace_delivered: DELIVERY_REPLACE_DELIVERED,
        }
    }
}

/// Result of hierarchical compose: the only sealed Final Transcript path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposeOutcome {
    decision: CompositionDecision,
    rendered: String,
    fallback_trigger: Option<FallbackTrigger>,
    error_codes: Vec<ComposeErrorCode>,
    delivery: DeliveryFlags,
}

impl ComposeOutcome {
    #[must_use]
    pub fn decision(&self) -> CompositionDecision {
        self.decision
    }

    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }

    #[must_use]
    pub fn fallback_trigger(&self) -> Option<FallbackTrigger> {
        self.fallback_trigger
    }

    #[must_use]
    pub fn error_codes(&self) -> &[ComposeErrorCode] {
        &self.error_codes
    }

    #[must_use]
    pub fn error_code_strs(&self) -> Vec<&'static str> {
        self.error_codes.iter().map(|c| c.as_str()).collect()
    }

    #[must_use]
    pub fn delivery(&self) -> DeliveryFlags {
        self.delivery
    }
}

// ─── Candidate / input types ─────────────────────────────────────────────────

/// STT source provider names used in the #139 research package.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SttProvider {
    ProviderA,
    ProviderB,
}

impl SttProvider {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderA => "provider_a",
            Self::ProviderB => "provider_b",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "provider_a" => Some(Self::ProviderA),
            "provider_b" => Some(Self::ProviderB),
            _ => None,
        }
    }
}

/// Host source-selection evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSelection {
    pub selected_provider: SttProvider,
    pub reason: String,
}

/// One available STT source transcript.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposeSource {
    pub provider: SttProvider,
    pub available: bool,
    pub text: String,
    pub primary: bool,
}

/// Certainty for removals / layout.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ComposeCertainty {
    Clear,
    Uncertain,
}

impl ComposeCertainty {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Uncertain => "uncertain",
        }
    }
}

/// Removal kind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RemovalKind {
    Filler,
    Backtrack,
}

/// Layout decision closed set.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LayoutDecision {
    Natural,
    MultiParagraph,
    Numbered,
    StructuredSections,
}

impl LayoutDecision {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Natural => "natural",
            Self::MultiParagraph => "multi_paragraph",
            Self::Numbered => "numbered",
            Self::StructuredSections => "structured_sections",
        }
    }
}

/// Derivation span kind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Keep,
    Remove,
    Convert,
    Label,
    LayoutBreak,
}

/// Untrusted structured candidate (typed). No free-form `final` field exists.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StructuredCandidate {
    pub schema_version: String,
    pub base_fingerprint: String,
    pub reconciliation: Reconciliation,
    pub removals: Vec<RemovalClaim>,
    pub conversions: Vec<ConversionClaim>,
    pub layout: LayoutClaim,
    pub labels: Vec<LabelClaim>,
    pub derivation: Vec<DerivationSpan>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Reconciliation {
    pub selected_provider: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RemovalClaim {
    pub kind: RemovalKind,
    pub certainty: ComposeCertainty,
    pub source_provider: String,
    pub source_span_text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConversionClaim {
    pub id: String,
    pub source_provider: String,
    pub source_span_text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LayoutClaim {
    pub decision: LayoutDecision,
    pub certainty: ComposeCertainty,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LabelClaim {
    pub label: String,
    pub source_provider: String,
    pub source_span_text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DerivationSpan {
    pub kind: SpanKind,
    /// Null for `layout_break`; required evidence for keep/remove/convert/label.
    pub source_provider: Option<String>,
    /// Empty only for `layout_break`; required for consuming spans (no silent default).
    pub source_text: String,
    pub output_text: String,
    pub conversion_id: Option<String>,
    pub label: Option<String>,
}

/// Inputs for [`compose_structured_candidate`].
///
/// All model influence arrives only as [`StructuredCandidate`] (or its absence).
/// There is no field for free-form model final prose.
///
/// `local_baseline` is a sealed [`LocalBaseline`] (host-organized text), never a
/// free model `&str`.
#[derive(Clone, Debug)]
pub struct ComposeInput<'a> {
    /// Deterministic local baseline (always Delivery-ready; host-sealed).
    pub local_baseline: &'a LocalBaseline,
    /// Host-selected source fingerprint (`sha256:…` of selected source UTF-8).
    pub base_fingerprint: &'a str,
    /// Available STT sources.
    pub sources: &'a [ComposeSource],
    /// Host selection (authoritative).
    pub source_selection: &'a SourceSelection,
    /// Exact protected substrings that must appear unchanged in accepted renders.
    pub protected_tokens: &'a [&'a str],
    /// Recording-start policy snapshot.
    pub policy: RenderingPolicy,
    /// Cloud attempt outcome.
    pub cloud_outcome: CloudOutcome,
    /// Structured candidate when cloud returned JSON; `None` when absent/unparsed.
    pub candidate: Option<&'a StructuredCandidate>,
}

// ─── JSON parse (optional entry) ─────────────────────────────────────────────

/// Parse untrusted candidate JSON into a typed [`StructuredCandidate`].
///
/// Fail-closed: returns `None` on invalid JSON, unknown fields, missing required
/// keys (including nullable Option fields that must still be present as JSON
/// `null`), oversize body, excessive depth/nodes, or over-limit claim/span
/// counts and field lengths. Caller passes `candidate: None` with
/// [`CloudOutcome::SchemaFailure`] (or similar).
#[must_use]
pub fn parse_structured_candidate_json(raw: &[u8]) -> Option<StructuredCandidate> {
    if raw.len() > MAX_COMPOSE_CANDIDATE_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(raw).ok()?;
    let (depth, nodes) = json_shape(&value);
    if depth > MAX_COMPOSE_JSON_DEPTH || nodes > MAX_COMPOSE_JSON_NODES {
        return None;
    }
    // Serde treats missing `Option` fields as `None`; product requires exact keys
    // (nullable keys present as JSON null). Enforce before typed deserialize.
    if !candidate_json_has_exact_keys(&value) {
        return None;
    }
    let candidate: StructuredCandidate = serde_json::from_value(value).ok()?;
    if !candidate_within_bounds(&candidate) {
        return None;
    }
    Some(candidate)
}

/// Exact required keys matching Python `exact_keys` / schema. Required keys must
/// be **present** (may be JSON `null` for nullable Option fields). Unknown keys
/// are already denied by `deny_unknown_fields` on typed deserialize.
fn candidate_json_has_exact_keys(value: &serde_json::Value) -> bool {
    const CANDIDATE_KEYS: &[&str] = &[
        "schema_version",
        "base_fingerprint",
        "reconciliation",
        "removals",
        "conversions",
        "layout",
        "labels",
        "derivation",
    ];
    const RECON_KEYS: &[&str] = &["selected_provider", "reason"];
    const REMOVAL_KEYS: &[&str] = &[
        "kind",
        "certainty",
        "source_provider",
        "source_span_text",
    ];
    const CONVERSION_KEYS: &[&str] = &["id", "source_provider", "source_span_text"];
    const LAYOUT_KEYS: &[&str] = &["decision", "certainty"];
    const LABEL_KEYS: &[&str] = &["label", "source_provider", "source_span_text"];
    const SPAN_KEYS: &[&str] = &[
        "kind",
        "source_provider",
        "source_text",
        "output_text",
        "conversion_id",
        "label",
    ];

    let Some(obj) = value.as_object() else {
        return false;
    };
    if !has_exact_keys(obj, CANDIDATE_KEYS) {
        return false;
    }
    let Some(recon) = obj.get("reconciliation").and_then(|v| v.as_object()) else {
        return false;
    };
    if !has_exact_keys(recon, RECON_KEYS) {
        return false;
    }
    let Some(layout) = obj.get("layout").and_then(|v| v.as_object()) else {
        return false;
    };
    if !has_exact_keys(layout, LAYOUT_KEYS) {
        return false;
    }
    for key in ["removals", "conversions", "labels", "derivation"] {
        let Some(arr) = obj.get(key).and_then(|v| v.as_array()) else {
            return false;
        };
        let expected = match key {
            "removals" => REMOVAL_KEYS,
            "conversions" => CONVERSION_KEYS,
            "labels" => LABEL_KEYS,
            "derivation" => SPAN_KEYS,
            _ => unreachable!(),
        };
        for item in arr {
            let Some(item_obj) = item.as_object() else {
                return false;
            };
            if !has_exact_keys(item_obj, expected) {
                return false;
            }
        }
    }
    true
}

fn has_exact_keys(
    object: &serde_json::Map<String, serde_json::Value>,
    expected: &[&str],
) -> bool {
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn json_shape(value: &serde_json::Value) -> (usize, usize) {
    match value {
        serde_json::Value::Array(values) => {
            let mut depth: usize = 1;
            let mut nodes: usize = 1;
            for child in values {
                let (child_depth, child_nodes) = json_shape(child);
                depth = depth.max(child_depth.saturating_add(1));
                nodes = nodes.saturating_add(child_nodes);
            }
            (depth, nodes)
        }
        serde_json::Value::Object(values) => {
            let mut depth: usize = 1;
            let mut nodes: usize = 1;
            for child in values.values() {
                let (child_depth, child_nodes) = json_shape(child);
                depth = depth.max(child_depth.saturating_add(1));
                nodes = nodes.saturating_add(child_nodes);
            }
            (depth, nodes)
        }
        _ => (1, 1),
    }
}

fn candidate_within_bounds(candidate: &StructuredCandidate) -> bool {
    if candidate.removals.len() > MAX_COMPOSE_REMOVALS
        || candidate.conversions.len() > MAX_COMPOSE_CONVERSIONS
        || candidate.labels.len() > MAX_COMPOSE_LABELS
        || candidate.derivation.len() > MAX_COMPOSE_DERIVATION_SPANS
    {
        return false;
    }
    if exceeds_field_bound(&candidate.schema_version)
        || exceeds_field_bound(&candidate.base_fingerprint)
        || exceeds_field_bound(&candidate.reconciliation.selected_provider)
        || exceeds_field_bound(&candidate.reconciliation.reason)
    {
        return false;
    }
    for removal in &candidate.removals {
        if exceeds_field_bound(&removal.source_provider)
            || exceeds_field_bound(&removal.source_span_text)
        {
            return false;
        }
    }
    for conversion in &candidate.conversions {
        if exceeds_field_bound(&conversion.id)
            || exceeds_field_bound(&conversion.source_provider)
            || exceeds_field_bound(&conversion.source_span_text)
        {
            return false;
        }
    }
    for label in &candidate.labels {
        if exceeds_field_bound(&label.label)
            || exceeds_field_bound(&label.source_provider)
            || exceeds_field_bound(&label.source_span_text)
        {
            return false;
        }
    }
    for span in &candidate.derivation {
        if exceeds_field_bound(&span.source_text) || exceeds_field_bound(&span.output_text) {
            return false;
        }
        if span
            .source_provider
            .as_deref()
            .is_some_and(exceeds_field_bound)
            || span
                .conversion_id
                .as_deref()
                .is_some_and(exceeds_field_bound)
            || span.label.as_deref().is_some_and(exceeds_field_bound)
        {
            return false;
        }
    }
    true
}

fn exceeds_field_bound(s: &str) -> bool {
    s.len() > MAX_COMPOSE_FIELD_UTF8_BYTES
}

// ─── Sole public compose entry ───────────────────────────────────────────────

/// Validate and hierarchically compose a structured candidate into a Final
/// Transcript, or fall back to the local baseline.
///
/// **Invariant:** this is the sole accept path for model text. Free-form model
/// prose cannot be passed in as the Final Transcript — only derivation spans
/// (after gates) or the host-supplied local baseline are rendered.
#[must_use]
pub fn compose_structured_candidate(input: &ComposeInput<'_>) -> ComposeOutcome {
    compose_impl(input)
}

fn outcome(
    decision: CompositionDecision,
    rendered: String,
    fallback_trigger: Option<FallbackTrigger>,
    error_codes: Vec<ComposeErrorCode>,
) -> ComposeOutcome {
    ComposeOutcome {
        decision,
        rendered,
        fallback_trigger,
        error_codes,
        delivery: DeliveryFlags::dpr_default(),
    }
}

fn baseline_result(
    baseline: &str,
    trigger: Option<FallbackTrigger>,
    codes: Vec<ComposeErrorCode>,
) -> ComposeOutcome {
    outcome(
        CompositionDecision::FallbackBaseline,
        baseline.to_owned(),
        trigger,
        codes,
    )
}

fn compose_impl(input: &ComposeInput<'_>) -> ComposeOutcome {
    let baseline = input.local_baseline.rendered();
    let outcome_cloud = input.cloud_outcome;
    let source_map = provider_text_map(input.sources);
    let selected = selected_source_text(input);
    let expected_fp = input.base_fingerprint;
    let closed: HashSet<&str> = CLOSED_CONVERSIONS.iter().copied().collect();

    // Pre-flight hard outcomes that ignore / skip candidate content.
    if outcome_cloud == CloudOutcome::Skipped {
        return baseline_result(baseline, None, vec![]);
    }
    if let Some((trigger, code)) = hard_outcome_map(outcome_cloud) {
        return baseline_result(baseline, Some(trigger), vec![code]);
    }

    let Some(candidate) = input.candidate else {
        // Missing candidate when not a pre-mapped hard outcome.
        return baseline_result(
            baseline,
            Some(FallbackTrigger::ResponseSchemaFailure),
            vec![ComposeErrorCode::Schema, ComposeErrorCode::Malformed],
        );
    };

    // Re-check bounds on any in-memory StructuredCandidate (not only parse path).
    // Public type can be constructed oversized; compose must not accept it.
    if !candidate_within_bounds(candidate) {
        return baseline_result(
            baseline,
            Some(FallbackTrigger::ResponseSchemaFailure),
            vec![ComposeErrorCode::Schema, ComposeErrorCode::Malformed],
        );
    }

    if let Some(codes) = validate_candidate_shape(candidate, &closed) {
        return baseline_result(
            baseline,
            Some(FallbackTrigger::ResponseSchemaFailure),
            codes,
        );
    }

    // Freshness
    if candidate.base_fingerprint != expected_fp {
        return baseline_result(
            baseline,
            Some(FallbackTrigger::UnverifiableSourceDerivation),
            vec![ComposeErrorCode::Stale],
        );
    }
    if let Some(sel) = selected.as_deref() {
        if text_sha256_fingerprint(sel) != expected_fp {
            return baseline_result(
                baseline,
                Some(FallbackTrigger::UnverifiableSourceDerivation),
                vec![ComposeErrorCode::Stale],
            );
        }
    }

    let recon = &candidate.reconciliation;
    let selection = input.source_selection;
    if recon.selected_provider != selection.selected_provider.as_str() {
        return baseline_result(
            baseline,
            Some(FallbackTrigger::UnsafeSemantics),
            vec![ComposeErrorCode::Reconcile, ComposeErrorCode::UnsafeSemantics],
        );
    }

    // Single-provider honesty
    let available_providers: Vec<&str> = input
        .sources
        .iter()
        .filter(|s| s.available)
        .map(|s| s.provider.as_str())
        .collect();
    if available_providers.len() == 1 {
        let only = available_providers[0];
        if recon.selected_provider != only {
            return baseline_result(
                baseline,
                Some(FallbackTrigger::UnsafeSemantics),
                vec![ComposeErrorCode::Reconcile, ComposeErrorCode::UnsafeSemantics],
            );
        }
        if (selection.reason == "only_available" || recon.reason == "only_available")
            && (recon.selected_provider != only || recon.reason != "only_available")
        {
            return baseline_result(
                baseline,
                Some(FallbackTrigger::UnsafeSemantics),
                vec![ComposeErrorCode::Reconcile, ComposeErrorCode::UnsafeSemantics],
            );
        }
    }

    let uncertain_backtrack = candidate
        .removals
        .iter()
        .any(|r| r.kind == RemovalKind::Backtrack && r.certainty == ComposeCertainty::Uncertain);
    let uncertain_layout = candidate.layout.certainty == ComposeCertainty::Uncertain;

    // Source-evidence for removals / conversions / labels
    for removal in &candidate.removals {
        let text = source_map
            .get(removal.source_provider.as_str())
            .map(String::as_str)
            .unwrap_or("");
        if !source_contains(text, &removal.source_span_text) {
            return baseline_result(
                baseline,
                Some(FallbackTrigger::UnverifiableSourceDerivation),
                vec![ComposeErrorCode::Unverifiable],
            );
        }
    }

    for conversion in &candidate.conversions {
        let text = source_map
            .get(conversion.source_provider.as_str())
            .map(String::as_str)
            .unwrap_or("");
        if !source_contains(text, &conversion.source_span_text) {
            return baseline_result(
                baseline,
                Some(FallbackTrigger::UnverifiableSourceDerivation),
                vec![ComposeErrorCode::Unverifiable],
            );
        }
        let cue = conversion_cue(&conversion.id);
        if !cue.is_empty() && !cue_covered_by(&cue, &conversion.source_span_text) {
            return baseline_result(
                baseline,
                Some(FallbackTrigger::UnverifiableSourceDerivation),
                vec![ComposeErrorCode::Unverifiable],
            );
        }
        if !cue.is_empty() && !cue_covered_by(&cue, text) {
            return baseline_result(
                baseline,
                Some(FallbackTrigger::UnverifiableSourceDerivation),
                vec![ComposeErrorCode::Unverifiable],
            );
        }
    }

    for label in &candidate.labels {
        let text = source_map
            .get(label.source_provider.as_str())
            .map(String::as_str)
            .unwrap_or("");
        if !source_contains(text, &label.source_span_text) {
            return baseline_result(
                baseline,
                Some(FallbackTrigger::UnverifiableSourceDerivation),
                vec![ComposeErrorCode::Unverifiable],
            );
        }
    }

    let declared_conversion_ids: HashSet<&str> =
        candidate.conversions.iter().map(|c| c.id.as_str()).collect();
    let declared_labels: HashSet<&str> =
        candidate.labels.iter().map(|l| l.label.as_str()).collect();
    let declared_removals: HashSet<(String, String)> = candidate
        .removals
        .iter()
        .map(|r| {
            (
                r.source_provider.clone(),
                normalize_ws(&r.source_span_text).to_lowercase(),
            )
        })
        .collect();
    let layout_decision = candidate.layout.decision;
    let layout_certainty = candidate.layout.certainty;
    let closed_label_set: HashSet<&str> = CLOSED_STRUCTURED_LABELS.iter().copied().collect();

    for span in &candidate.derivation {
        match span.kind {
            SpanKind::LayoutBreak => {
                let out_lb = span.output_text.as_str();
                if !matches!(out_lb, "\n" | "\n\n" | " " | "\t" | "")
                    && !out_lb.chars().all(|c| matches!(c, '\n' | '\r' | '\t' | ' '))
                {
                    return baseline_result(
                        baseline,
                        Some(FallbackTrigger::UnverifiableSourceDerivation),
                        vec![ComposeErrorCode::Unverifiable],
                    );
                }
                if layout_decision == LayoutDecision::Natural
                    && layout_certainty == ComposeCertainty::Clear
                    && is_multiparagraph_text(out_lb)
                {
                    return baseline_result(
                        baseline,
                        Some(FallbackTrigger::UnsafeSemantics),
                        vec![ComposeErrorCode::UnsafeSemantics],
                    );
                }
            }
            _ => {
                let provider = span.source_provider.as_deref().unwrap_or("");
                let source_text = span.source_text.as_str();
                if !source_map.contains_key(provider) {
                    return baseline_result(
                        baseline,
                        Some(FallbackTrigger::UnverifiableSourceDerivation),
                        vec![ComposeErrorCode::Unverifiable],
                    );
                }
                if !source_contains(&source_map[provider], source_text) {
                    return baseline_result(
                        baseline,
                        Some(FallbackTrigger::UnverifiableSourceDerivation),
                        vec![ComposeErrorCode::Unverifiable],
                    );
                }
                if span.kind == SpanKind::Remove {
                    if !span.output_text.is_empty() {
                        return baseline_result(
                            baseline,
                            Some(FallbackTrigger::ResponseSchemaFailure),
                            vec![ComposeErrorCode::Malformed],
                        );
                    }
                    let rem_key = (provider.to_owned(), normalize_ws(source_text).to_lowercase());
                    if !declared_removals.contains(&rem_key) {
                        return baseline_result(
                            baseline,
                            Some(FallbackTrigger::UnverifiableSourceDerivation),
                            vec![ComposeErrorCode::Unverifiable],
                        );
                    }
                }
                if span.kind == SpanKind::Convert {
                    let Some(cid) = span.conversion_id.as_deref() else {
                        return baseline_result(
                            baseline,
                            Some(FallbackTrigger::ResponseSchemaFailure),
                            vec![ComposeErrorCode::UnknownConversion],
                        );
                    };
                    if !closed.contains(cid) {
                        return baseline_result(
                            baseline,
                            Some(FallbackTrigger::ResponseSchemaFailure),
                            vec![ComposeErrorCode::UnknownConversion],
                        );
                    }
                    if !declared_conversion_ids.contains(cid) {
                        return baseline_result(
                            baseline,
                            Some(FallbackTrigger::UnverifiableSourceDerivation),
                            vec![ComposeErrorCode::Unverifiable],
                        );
                    }
                    let cue = conversion_cue(cid);
                    if !cue.is_empty() && !cue_covered_by(&cue, source_text) {
                        return baseline_result(
                            baseline,
                            Some(FallbackTrigger::UnverifiableSourceDerivation),
                            vec![ComposeErrorCode::Unverifiable],
                        );
                    }
                    if layout_decision == LayoutDecision::Natural
                        && layout_certainty == ComposeCertainty::Clear
                        && is_multiparagraph_text(&conversion_rhs(cid))
                    {
                        return baseline_result(
                            baseline,
                            Some(FallbackTrigger::UnsafeSemantics),
                            vec![ComposeErrorCode::UnsafeSemantics],
                        );
                    }
                }
                if span.kind == SpanKind::Label {
                    let lab = span.label.as_deref().unwrap_or("");
                    if !closed_label_set.contains(lab) {
                        return baseline_result(
                            baseline,
                            Some(FallbackTrigger::InvalidFixedLabel),
                            vec![ComposeErrorCode::InvalidLabel],
                        );
                    }
                    if !declared_labels.contains(lab) {
                        return baseline_result(
                            baseline,
                            Some(FallbackTrigger::InvalidFixedLabel),
                            vec![ComposeErrorCode::InvalidLabel],
                        );
                    }
                    let out = span.output_text.as_str();
                    // Contract: label output_text must be the exact closed header
                    // form `Label:\n` or `Label:` (not a prefix check). Product is
                    // stricter than the Python oracle's starts_with here so a fat
                    // label span cannot rewrite body words — body must be separate
                    // ordered keep/convert spans; source_text should cover the
                    // spoken cue only.
                    let header_nl = format!("{lab}:\n");
                    let header_inline = format!("{lab}:");
                    if out != header_nl && out != header_inline {
                        return baseline_result(
                            baseline,
                            Some(FallbackTrigger::InvalidFixedLabel),
                            vec![ComposeErrorCode::InvalidLabel],
                        );
                    }
                }
            }
        }
    }

    let composed = compose_render(&candidate.derivation);

    if layout_decision == LayoutDecision::Natural
        && layout_certainty == ComposeCertainty::Clear
        && is_multiparagraph_text(&composed)
    {
        return baseline_result(
            baseline,
            Some(FallbackTrigger::UnsafeSemantics),
            vec![ComposeErrorCode::UnsafeSemantics],
        );
    }

    let headers = structural_headers(&composed);
    for header in &headers {
        let exact = closed_label_set.contains(header.as_str());
        let case_ok = CLOSED_STRUCTURED_LABELS
            .iter()
            .any(|lab| lab.eq_ignore_ascii_case(header));
        if !exact && !case_ok {
            return baseline_result(
                baseline,
                Some(FallbackTrigger::InvalidFixedLabel),
                vec![ComposeErrorCode::InvalidLabel],
            );
        }
        if !exact {
            return baseline_result(
                baseline,
                Some(FallbackTrigger::InvalidFixedLabel),
                vec![ComposeErrorCode::InvalidLabel],
            );
        }
    }

    if input.policy == RenderingPolicy::Natural && !headers.is_empty() {
        return baseline_result(
            baseline,
            Some(FallbackTrigger::InvalidFixedLabel),
            vec![ComposeErrorCode::InvalidLabel],
        );
    }

    // Protected tokens before invented-content when not on soft path
    if !uncertain_backtrack && !uncertain_layout {
        for token in input.protected_tokens {
            if !token.is_empty() && !composed.contains(token) {
                return baseline_result(
                    baseline,
                    Some(FallbackTrigger::UnsafeSemantics),
                    vec![ComposeErrorCode::Protected, ComposeErrorCode::UnsafeSemantics],
                );
            }
        }
    }

    let declared_conv_list: Vec<&str> = declared_conversion_ids.iter().copied().collect();
    let declared_lab_list: Vec<&str> = declared_labels.iter().copied().collect();
    let allowed = licensed_atoms(&source_map, &declared_conv_list, &declared_lab_list);
    let out_atoms = lexical_atoms(&composed);
    let invented: Vec<_> = out_atoms
        .iter()
        .filter(|a| !allowed.contains(a.as_str()))
        .collect();
    if !invented.is_empty() {
        return baseline_result(
            baseline,
            Some(FallbackTrigger::UnsafeSemantics),
            vec![
                ComposeErrorCode::InventedContent,
                ComposeErrorCode::UnsafeSemantics,
            ],
        );
    }

    // Soft salvage
    if uncertain_backtrack {
        let rendered = baseline.to_owned();
        for token in input.protected_tokens {
            if !token.is_empty() && !rendered.contains(token) {
                return baseline_result(
                    baseline,
                    Some(FallbackTrigger::UnsafeSemantics),
                    vec![ComposeErrorCode::Protected, ComposeErrorCode::UnsafeSemantics],
                );
            }
        }
        return outcome(
            CompositionDecision::AcceptPreserveWords,
            rendered,
            Some(FallbackTrigger::UncertainBacktracking),
            vec![ComposeErrorCode::UncertainBacktrack],
        );
    }

    if uncertain_layout {
        let rendered = if !baseline.is_empty() {
            baseline.to_owned()
        } else {
            natural_layout_render(candidate)
        };
        for token in input.protected_tokens {
            if !token.is_empty() && !rendered.contains(token) {
                return baseline_result(
                    baseline,
                    Some(FallbackTrigger::UnsafeSemantics),
                    vec![ComposeErrorCode::Protected, ComposeErrorCode::UnsafeSemantics],
                );
            }
        }
        return outcome(
            CompositionDecision::AcceptNaturalLayout,
            rendered,
            Some(FallbackTrigger::UncertainLayout),
            vec![ComposeErrorCode::UncertainLayout],
        );
    }

    for token in input.protected_tokens {
        if !token.is_empty() && !composed.contains(token) {
            return baseline_result(
                baseline,
                Some(FallbackTrigger::UnsafeSemantics),
                vec![ComposeErrorCode::Protected, ComposeErrorCode::UnsafeSemantics],
            );
        }
    }

    let selected_provider = Some(selection.selected_provider.as_str());
    let placement_codes =
        claim_source_ranges(&source_map, &candidate.derivation, selected_provider);
    if !placement_codes.is_empty() {
        return baseline_result(
            baseline,
            Some(FallbackTrigger::UnverifiableSourceDerivation),
            placement_codes,
        );
    }

    for span in &candidate.derivation {
        let source_text = span.source_text.as_str();
        let output_text = span.output_text.as_str();
        if span.kind == SpanKind::Keep && !keep_organize_only(source_text, output_text) {
            return baseline_result(
                baseline,
                Some(FallbackTrigger::UnsafeSemantics),
                vec![
                    ComposeErrorCode::InventedContent,
                    ComposeErrorCode::UnsafeSemantics,
                ],
            );
        }
        if span.kind == SpanKind::Convert {
            let Some(cid) = span.conversion_id.as_deref() else {
                return baseline_result(
                    baseline,
                    Some(FallbackTrigger::UnsafeSemantics),
                    vec![
                        ComposeErrorCode::InventedContent,
                        ComposeErrorCode::UnsafeSemantics,
                    ],
                );
            };
            if !convert_output_matches(cid, source_text, output_text) {
                return baseline_result(
                    baseline,
                    Some(FallbackTrigger::UnsafeSemantics),
                    vec![
                        ComposeErrorCode::InventedContent,
                        ComposeErrorCode::UnsafeSemantics,
                    ],
                );
            }
        }
    }

    outcome(
        CompositionDecision::Accept,
        composed,
        None,
        vec![],
    )
}

fn hard_outcome_map(outcome: CloudOutcome) -> Option<(FallbackTrigger, ComposeErrorCode)> {
    match outcome {
        CloudOutcome::SchemaFailure => {
            Some((FallbackTrigger::ResponseSchemaFailure, ComposeErrorCode::Schema))
        }
        CloudOutcome::ProviderFailure => {
            Some((FallbackTrigger::ProviderFailure, ComposeErrorCode::Provider))
        }
        CloudOutcome::DeadlineExceeded => {
            Some((FallbackTrigger::DeadlineExceeded, ComposeErrorCode::Deadline))
        }
        _ => None,
    }
}

// ─── Shape validation ────────────────────────────────────────────────────────

fn validate_candidate_shape(
    candidate: &StructuredCandidate,
    closed_conversions: &HashSet<&str>,
) -> Option<Vec<ComposeErrorCode>> {
    let mut unknown_conversion = false;
    let mut unknown_label = false;
    let mut malformed = false;

    if candidate.schema_version != "1" {
        malformed = true;
    }
    if !is_fingerprint(&candidate.base_fingerprint) {
        malformed = true;
    }
    if SttProvider::parse(&candidate.reconciliation.selected_provider).is_none() {
        malformed = true;
    }
    if !is_select_reason(&candidate.reconciliation.reason) {
        malformed = true;
    }
    for conversion in &candidate.conversions {
        if !closed_conversions.contains(conversion.id.as_str()) {
            unknown_conversion = true;
        }
        if SttProvider::parse(&conversion.source_provider).is_none()
            || conversion.source_span_text.is_empty()
        {
            malformed = true;
        }
    }
    for label in &candidate.labels {
        if !CLOSED_STRUCTURED_LABELS.contains(&label.label.as_str()) {
            unknown_label = true;
        }
        if SttProvider::parse(&label.source_provider).is_none() || label.source_span_text.is_empty()
        {
            malformed = true;
        }
    }
    for removal in &candidate.removals {
        if SttProvider::parse(&removal.source_provider).is_none()
            || removal.source_span_text.is_empty()
        {
            malformed = true;
        }
    }
    if candidate.derivation.is_empty() {
        malformed = true;
    }
    for span in &candidate.derivation {
        if let Some(ref p) = span.source_provider {
            if SttProvider::parse(p).is_none() {
                malformed = true;
            }
        }
        // Prototype: derivation.label must be null or a closed Structured label.
        if let Some(ref lab) = span.label {
            if !CLOSED_STRUCTURED_LABELS.contains(&lab.as_str()) {
                malformed = true;
            }
        }
    }

    if !unknown_conversion && !unknown_label && !malformed {
        return None;
    }
    // Match Python last-write: when both unknown conversion and unknown label
    // are present, label codes win.
    if unknown_label {
        return Some(vec![
            ComposeErrorCode::UnknownLabel,
            ComposeErrorCode::Malformed,
        ]);
    }
    if unknown_conversion {
        return Some(vec![
            ComposeErrorCode::UnknownConversion,
            ComposeErrorCode::Malformed,
        ]);
    }
    Some(vec![ComposeErrorCode::Malformed])
}

fn is_fingerprint(value: &str) -> bool {
    crate::is_text_sha256_fingerprint(value)
}

fn is_select_reason(value: &str) -> bool {
    matches!(
        value,
        "only_available"
            | "exact_agreement"
            | "configured_primary_rank"
            | "punctuation_local_render"
            | "safe_complementary_merge"
    )
}

// ─── Helpers (faithful port) ─────────────────────────────────────────────────

fn provider_text_map(sources: &[ComposeSource]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for source in sources {
        if source.available {
            out.insert(source.provider.as_str().to_owned(), source.text.clone());
        }
    }
    out
}

fn selected_source_text(input: &ComposeInput<'_>) -> Option<String> {
    let provider = input.source_selection.selected_provider;
    input
        .sources
        .iter()
        .find(|s| s.provider == provider && s.available)
        .map(|s| s.text.clone())
}

fn normalize_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_owned()
}

fn source_contains(source_text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    if source_text.contains(needle) {
        return true;
    }
    normalize_ws(source_text)
        .to_lowercase()
        .contains(&normalize_ws(needle).to_lowercase())
}

fn atom_re() -> Regex {
    Regex::new(r"[A-Za-z0-9]+(?:[_./=+\-][A-Za-z0-9]+)*").expect("atom regex")
}

fn lexical_atoms(text: &str) -> BTreeSet<String> {
    atom_re()
        .find_iter(text)
        .map(|m| m.as_str().to_lowercase())
        .collect()
}

fn lexical_atom_sequence(text: &str) -> Vec<String> {
    atom_re()
        .find_iter(text)
        .map(|m| m.as_str().to_lowercase())
        .collect()
}

fn structural_headers(text: &str) -> Vec<String> {
    let re = Regex::new(r"^[ \t]*([A-Za-z][A-Za-z0-9 ]*):(.*)$").expect("header regex");
    let closed_cf: HashMap<String, &str> = CLOSED_STRUCTURED_LABELS
        .iter()
        .map(|l| (l.to_lowercase(), *l))
        .collect();
    let mut found = Vec::new();
    for line in text.lines() {
        let Some(caps) = re.captures(line) else {
            continue;
        };
        let raw = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        let body = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        // Python: closed is not None or not body.strip() or raw.istitle()
        let is_closed = closed_cf.contains_key(&raw.to_lowercase());
        if is_closed || body.trim().is_empty() || is_title_case(raw) {
            found.push(raw.to_owned());
        }
    }
    found
}

fn is_title_case(s: &str) -> bool {
    // Python str.istitle(): first char of each word upper, rest lower; words split on non-alpha.
    if s.is_empty() {
        return false;
    }
    s.chars().any(|c| c.is_alphabetic()) && {
        let mut word_start = true;
        let mut saw_cased = false;
        for ch in s.chars() {
            if !ch.is_alphabetic() {
                word_start = true;
                continue;
            }
            if word_start {
                if !ch.is_uppercase() {
                    return false;
                }
                saw_cased = true;
                word_start = false;
            } else if ch.is_uppercase() {
                return false;
            }
        }
        saw_cased
    }
}

fn conversion_rhs(conversion_id: &str) -> String {
    let Some((_, rhs_raw)) = conversion_id.split_once('→') else {
        return String::new();
    };
    let mut rhs = rhs_raw.trim().to_owned();
    if rhs.to_lowercase().ends_with(" label") {
        let cut = rhs.len() - " label".len();
        rhs.truncate(cut);
    }
    if rhs == "\\n" {
        return "\n".to_owned();
    }
    if rhs == "\\n\\n" {
        return "\n\n".to_owned();
    }
    if rhs.eq_ignore_ascii_case("numbered_lines") {
        return String::new();
    }
    rhs
}

fn conversion_cue(conversion_id: &str) -> String {
    match conversion_id.split_once('→') {
        Some((cue, _)) => cue.trim().to_owned(),
        None => conversion_id.to_owned(),
    }
}

fn cue_needles(cue: &str) -> Vec<String> {
    if cue.is_empty() {
        return vec![];
    }
    let re = Regex::new(r"(?i)^spoken\s+(.+?)\s+cue$").expect("spoken cue");
    if let Some(caps) = re.captures(cue) {
        return vec![caps.get(1).unwrap().as_str().trim().to_owned()];
    }
    if cue.contains('…') {
        return cue
            .split('…')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
    }
    vec![cue.to_owned()]
}

fn cue_covered_by(cue: &str, text: &str) -> bool {
    let needles = cue_needles(cue);
    !needles.is_empty() && needles.iter().all(|n| source_contains(text, n))
}

fn keep_organize_only(source_text: &str, output_text: &str) -> bool {
    lexical_atom_sequence(source_text) == lexical_atom_sequence(output_text)
}

fn convert_output_matches(conversion_id: &str, source_text: &str, output_text: &str) -> bool {
    let rhs = conversion_rhs(conversion_id);
    let rhs_template = conversion_id
        .split_once('→')
        .map(|(_, r)| r.trim())
        .unwrap_or("");

    if rhs_template.contains('…') {
        let re = Regex::new(r"(?is)quote\s+(.+?)\s+unquote").expect("quote re");
        let Some(caps) = re.captures(source_text) else {
            return false;
        };
        let interior = normalize_ws(caps.get(1).unwrap().as_str());
        let expected = rhs_template.replace('…', &interior);
        return normalize_ws(output_text) == normalize_ws(&expected);
    }

    if rhs == "\n" || rhs == "\n\n" {
        return output_text == rhs;
    }
    if rhs.is_empty() {
        return output_text.is_empty();
    }
    let stripped = output_text.trim();
    if stripped == rhs {
        return true;
    }
    if output_text.trim_end() == rhs || output_text.trim_start() == rhs {
        return true;
    }
    false
}

/// Case-insensitive literal occurrences of `needle` in `haystack` as original
/// UTF-8 byte ranges `(start, end)` on char boundaries.
///
/// Never panics on valid Unicode: does not slice mid-codepoint and does not
/// map casefold byte offsets onto the original string (casefold can expand or
/// contract). On unverifiable matches returns empty rather than wrong ranges.
///
/// Collects **exact and case-insensitive** char-window matches together (then
/// dedupes ranges) so e.g. haystack `foo FOO` with needle `foo` yields both
/// non-overlapping ranges. Falls to whitespace-atom matching only when both
/// prior paths found nothing.
fn find_literal_spans(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() || haystack.is_empty() {
        return vec![];
    }

    let mut out = Vec::new();

    // Exact (case-sensitive) — offsets are identity on the original.
    {
        let mut search_from = 0;
        while search_from <= haystack.len() {
            let Some(rel) = haystack[search_from..].find(needle) else {
                break;
            };
            let start = search_from + rel;
            let end = start + needle.len();
            debug_assert!(haystack.is_char_boundary(start) && haystack.is_char_boundary(end));
            out.push((start, end));
            // Advance by one original char so multi-byte starts stay in bounds.
            let step = haystack[start..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            search_from = start + step;
        }
    }

    // Case-insensitive char-window match: compare per-char lowercase expansions
    // but claim original char-boundary ranges (never casefold offsets).
    // Always run (do not early-return after exact-only) so mixed-case duplicates
    // like `foo FOO` both resolve.
    let needle_chars: Vec<char> = needle.chars().collect();
    let hay_chars: Vec<(usize, char)> = haystack.char_indices().collect();
    let n_len = needle_chars.len();
    if n_len > 0 && hay_chars.len() >= n_len {
        for i in 0..=hay_chars.len() - n_len {
            let equal = needle_chars
                .iter()
                .zip(hay_chars[i..i + n_len].iter().map(|(_, c)| c))
                .all(|(nc, hc)| chars_eq_ignore_case(*nc, *hc));
            if equal {
                let start = hay_chars[i].0;
                let end = if i + n_len < hay_chars.len() {
                    hay_chars[i + n_len].0
                } else {
                    haystack.len()
                };
                debug_assert!(haystack.is_char_boundary(start) && haystack.is_char_boundary(end));
                out.push((start, end));
            }
        }
    }

    // Dedupe ranges from exact + case-insensitive paths.
    out.sort_by_key(|r| (r.0, r.1));
    out.dedup();
    if !out.is_empty() {
        return out;
    }

    // Whitespace-normalized multi-word fallback via lexical atom runs.
    let norm_n = normalize_ws(needle).to_lowercase();
    let norm_h = normalize_ws(haystack).to_lowercase();
    if norm_n.is_empty() || !norm_h.contains(&norm_n) {
        return out;
    }
    let atoms = lexical_atom_sequence(needle);
    if atoms.is_empty() {
        return out;
    }
    let re = atom_re();
    let h_atoms: Vec<_> = re.find_iter(haystack).collect();
    let seq: Vec<String> = h_atoms
        .iter()
        .map(|m| m.as_str().to_lowercase())
        .collect();
    // Fail closed when haystack has fewer atoms than needle (e.g. "İ" vs
    // "i\u{307}" where norm contains matches but ASCII atom seq is empty).
    // Never index empty `seq` / never `seq[i..i+atoms.len()]` when short.
    if seq.len() < atoms.len() {
        return out;
    }
    for i in 0..=seq.len() - atoms.len() {
        if seq[i..i + atoms.len()] == atoms[..] {
            let a0 = h_atoms[i].start();
            let a1 = h_atoms[i + atoms.len() - 1].end();
            if haystack.is_char_boundary(a0) && haystack.is_char_boundary(a1) {
                out.push((a0, a1));
            }
        }
    }
    out
}

fn chars_eq_ignore_case(a: char, b: char) -> bool {
    if a == b || a.eq_ignore_ascii_case(&b) {
        return true;
    }
    // Full Unicode: compare lowercase iterators (may be multi-char, e.g. İ).
    a.to_lowercase().eq(b.to_lowercase())
}

fn ranges_overlap(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

fn claim_source_ranges(
    source_map: &BTreeMap<String, String>,
    derivation: &[DerivationSpan],
    selected_provider: Option<&str>,
) -> Vec<ComposeErrorCode> {
    let mut claimed: HashMap<String, Vec<(usize, usize)>> = source_map
        .keys()
        .map(|p| (p.clone(), Vec::new()))
        .collect();
    let mut prev_start: HashMap<String, isize> =
        source_map.keys().map(|p| (p.clone(), -1isize)).collect();

    for span in derivation {
        if !matches!(
            span.kind,
            SpanKind::Keep | SpanKind::Remove | SpanKind::Convert | SpanKind::Label
        ) {
            continue;
        }
        let provider = span.source_provider.as_deref().unwrap_or("");
        let source_text = span.source_text.as_str();
        if !source_map.contains_key(provider) || source_text.is_empty() {
            continue;
        }
        let hay = &source_map[provider];
        let mut candidates = find_literal_spans(hay, source_text);
        candidates.sort_by_key(|r| r.0);
        if candidates.is_empty() {
            continue;
        }
        let mut placed: Option<(usize, usize)> = None;
        for cand in candidates {
            let used = &claimed[provider];
            if used.iter().any(|u| ranges_overlap(cand, *u)) {
                continue;
            }
            placed = Some(cand);
            break;
        }
        let Some((start, end)) = placed else {
            return vec![ComposeErrorCode::Overlap];
        };
        if (start as isize) < prev_start[provider] {
            return vec![ComposeErrorCode::Unverifiable];
        }
        prev_start.insert(provider.to_owned(), start as isize);
        claimed.get_mut(provider).unwrap().push((start, end));
    }

    let mut providers_to_cover: HashSet<String> = HashSet::new();
    if let Some(sp) = selected_provider {
        if source_map.contains_key(sp) {
            providers_to_cover.insert(sp.to_owned());
        }
    }
    for (provider, ranges) in &claimed {
        if !ranges.is_empty() {
            providers_to_cover.insert(provider.clone());
        }
    }

    for provider in providers_to_cover {
        let text = &source_map[&provider];
        if text.is_empty() {
            continue;
        }
        let mut covered = vec![false; text.len()];
        for &(start, end) in &claimed[&provider] {
            let lo = start.min(text.len());
            let hi = end.min(text.len());
            for i in lo..hi {
                covered[i] = true;
            }
        }
        for (i, ch) in text.char_indices() {
            // Mark all bytes of the char if first byte covered; for whitespace
            // we only care about non-whitespace bytes.
            if ch.is_whitespace() {
                continue;
            }
            // char_indices gives char start; for multi-byte, check start only
            // (research fixtures are ASCII).
            if !covered[i] {
                return vec![ComposeErrorCode::Unverifiable];
            }
        }
    }
    vec![]
}

fn licensed_atoms(
    sources: &BTreeMap<String, String>,
    conversions: &[&str],
    labels: &[&str],
) -> HashSet<String> {
    let mut allowed = HashSet::new();
    for text in sources.values() {
        allowed.extend(lexical_atoms(text));
    }
    for conversion_id in conversions {
        let rhs = conversion_rhs(conversion_id);
        if !rhs.is_empty() && rhs != "\n" && rhs != "\n\n" {
            allowed.extend(lexical_atoms(&rhs));
        }
        allowed.extend(lexical_atoms(&conversion_cue(conversion_id)));
    }
    for label in labels {
        allowed.extend(lexical_atoms(label));
    }
    allowed
}

fn compose_render(derivation: &[DerivationSpan]) -> String {
    derivation
        .iter()
        .map(|s| s.output_text.as_str())
        .collect()
}

fn is_multiparagraph_text(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    Regex::new(r"\n[ \t]*\n")
        .expect("mp re")
        .is_match(&normalized)
}

fn natural_layout_render(candidate: &StructuredCandidate) -> String {
    let mut parts = String::new();
    for span in &candidate.derivation {
        if span.kind == SpanKind::LayoutBreak {
            parts.push(' ');
            continue;
        }
        parts.push_str(&span.output_text);
    }
    let re = Regex::new(r"[ \t]+").expect("ws re");
    re.replace_all(&parts, " ").trim().to_owned()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::OnceLock;

    const CORPUS_JSON: &str = include_str!(
        "../../../docs/research/developer-prompt-rendering-combined-call-corpus-2026-08-11.json"
    );

    fn corpus() -> &'static Value {
        static CORPUS: OnceLock<Value> = OnceLock::new();
        CORPUS.get_or_init(|| serde_json::from_str(CORPUS_JSON).expect("corpus JSON"))
    }

    fn policy_from(s: &str) -> RenderingPolicy {
        RenderingPolicy::parse(s).expect("policy")
    }

    fn cloud_from(s: &str) -> CloudOutcome {
        CloudOutcome::parse(s).expect("cloud_outcome")
    }

    fn provider_from(s: &str) -> SttProvider {
        SttProvider::parse(s).expect("provider")
    }

    fn fixture_input(fx: &Value) -> (ComposeInputOwned, Option<StructuredCandidate>) {
        let sources: Vec<ComposeSource> = fx["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| ComposeSource {
                provider: provider_from(s["provider"].as_str().unwrap()),
                available: s["available"].as_bool().unwrap(),
                text: s["text"].as_str().unwrap_or("").to_owned(),
                primary: s["primary"].as_bool().unwrap_or(false),
            })
            .collect();
        let selection = SourceSelection {
            selected_provider: provider_from(
                fx["source_selection"]["selected_provider"]
                    .as_str()
                    .unwrap(),
            ),
            reason: fx["source_selection"]["reason"]
                .as_str()
                .unwrap()
                .to_owned(),
        };
        let protected: Vec<String> = fx["protected_tokens"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|t| t.as_str().map(str::to_owned))
            .collect();
        let candidate = if fx["candidate"].is_null() {
            None
        } else {
            Some(
                serde_json::from_value::<StructuredCandidate>(fx["candidate"].clone())
                    .unwrap_or_else(|e| panic!("candidate parse for {}: {e}", fx["id"])),
            )
        };
        let owned = ComposeInputOwned {
            local_baseline: LocalBaseline::from_organized_text(
                fx["local_baseline"].as_str().unwrap(),
            ),
            base_fingerprint: fx["base_fingerprint"].as_str().unwrap().to_owned(),
            sources,
            source_selection: selection,
            protected_tokens: protected,
            policy: policy_from(fx["policy"].as_str().unwrap()),
            cloud_outcome: cloud_from(fx["cloud_outcome"].as_str().unwrap()),
        };
        (owned, candidate)
    }

    struct ComposeInputOwned {
        local_baseline: LocalBaseline,
        base_fingerprint: String,
        sources: Vec<ComposeSource>,
        source_selection: SourceSelection,
        protected_tokens: Vec<String>,
        policy: RenderingPolicy,
        cloud_outcome: CloudOutcome,
    }

    fn compose_owned(
        owned: &ComposeInputOwned,
        candidate: Option<&StructuredCandidate>,
    ) -> ComposeOutcome {
        let protected: Vec<&str> = owned.protected_tokens.iter().map(String::as_str).collect();
        let input = ComposeInput {
            local_baseline: &owned.local_baseline,
            base_fingerprint: &owned.base_fingerprint,
            sources: &owned.sources,
            source_selection: &owned.source_selection,
            protected_tokens: &protected,
            policy: owned.policy,
            cloud_outcome: owned.cloud_outcome,
            candidate,
        };
        compose_structured_candidate(&input)
    }

    #[test]
    fn closed_conversions_match_corpus_catalog() {
        let c = corpus();
        let catalog: Vec<&str> = c["closed_conversions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(catalog, CLOSED_CONVERSIONS);
        assert_eq!(CLOSED_STRUCTURED_LABELS.len(), 8);
    }

    #[test]
    fn corpus_all_24_decisions_and_rendered_match() {
        let c = corpus();
        assert_eq!(c["version"].as_str().unwrap(), "1.1.2");
        let fixtures = c["fixtures"].as_array().unwrap();
        assert_eq!(fixtures.len(), 24, "expected full #139 corpus (24 fixtures)");

        let mut mismatches = Vec::new();
        for fx in fixtures {
            let id = fx["id"].as_str().unwrap();
            let (owned, candidate) = fixture_input(fx);
            let got = compose_owned(&owned, candidate.as_ref());
            let exp = &fx["expected"];
            let exp_decision = exp["decision"].as_str().unwrap();
            let exp_rendered = exp["rendered"].as_str().unwrap();
            let exp_trigger = exp["fallback_trigger"].as_str();
            let exp_codes: Vec<&str> = exp["error_codes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c.as_str().unwrap())
                .collect();
            let got_trigger = got.fallback_trigger().map(|t| t.as_str());
            let got_codes = got.error_code_strs();

            let delivery = got.delivery();
            let del_ok = delivery.state == DELIVERY_STATE_UNSENT
                && !delivery.auto_send
                && !delivery.live_type
                && !delivery.replace_delivered;

            if got.decision().as_str() != exp_decision
                || got.rendered() != exp_rendered
                || got_trigger != exp_trigger
                || got_codes != exp_codes
                || !del_ok
            {
                mismatches.push(format!(
                    "{id}: decision got={} want={} | trigger got={got_trigger:?} want={exp_trigger:?} | codes got={got_codes:?} want={exp_codes:?} | rendered_eq={} | delivery_ok={del_ok}\n  got_rendered={:?}\n  want_rendered={:?}",
                    got.decision().as_str(),
                    exp_decision,
                    got.rendered() == exp_rendered,
                    got.rendered(),
                    exp_rendered,
                ));
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} fixture(s) failed:\n{}",
            mismatches.len(),
            mismatches.join("\n\n")
        );
    }

    fn clone_fixture(id: &str) -> Value {
        let c = corpus();
        c["fixtures"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == id)
            .cloned()
            .expect("fixture")
    }

    fn run_fx(mut fx: Value) -> ComposeOutcome {
        let (owned, candidate) = fixture_input(&fx);
        // re-parse after possible mutation — fixture_input already used fx
        let _ = &mut fx;
        compose_owned(&owned, candidate.as_ref())
    }

    #[test]
    fn mutation_missing_derivation_span_rejects() {
        let mut fx = clone_fixture("CC-01");
        // Keep only first span — omits convert covering "exclamation point"
        let first = fx["candidate"]["derivation"][0].clone();
        fx["candidate"]["derivation"] = Value::Array(vec![first]);
        let got = run_fx(fx);
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert!(got
            .error_codes()
            .contains(&ComposeErrorCode::Unverifiable));
    }

    #[test]
    fn mutation_reordered_source_spans_reject() {
        let mut fx = clone_fixture("CC-18");
        fx["candidate"]["removals"] = Value::Array(vec![]);
        fx["candidate"]["derivation"] = serde_json::json!([
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "when you get a chance",
                "output_text": "When you get a chance ",
                "conversion_id": null,
                "label": null
            },
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hey can you send the notes",
                "output_text": "Hey can you send the notes",
                "conversion_id": null,
                "label": null
            }
        ]);
        let got = run_fx(fx);
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert!(got
            .error_codes()
            .contains(&ComposeErrorCode::Unverifiable));
    }

    #[test]
    fn mutation_protected_token_altered_rejects() {
        let mut fx = clone_fixture("CC-14");
        fx["cloud_outcome"] = Value::String("succeeded".into());
        fx["candidate"]["derivation"] = serde_json::json!([{
            "kind": "keep",
            "source_provider": "provider_a",
            "source_text": "call Anuraj about the release",
            "output_text": "Call anuraj about the release.",
            "conversion_id": null,
            "label": null
        }]);
        let got = run_fx(fx);
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert!(got.error_codes().contains(&ComposeErrorCode::Protected));
    }

    #[test]
    fn mutation_illegal_label_rejects() {
        // Mutate green structured fixture (CC-20), not the stock reject fixture.
        let mut fx = clone_fixture("CC-20");
        fx["candidate"]["derivation"][0]["output_text"] = Value::String("Edge Cases:\n".into());
        let got = run_fx(fx);
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert!(got.error_codes().contains(&ComposeErrorCode::InvalidLabel));
    }

    /// Fat label span: claims full source and rewrites body into `output_text`.
    /// Product rejects with exact `InvalidLabel` (not a soft set of codes);
    /// Python oracle may still accept via starts_with.
    #[test]
    fn mutation_label_fat_rewrite_body_rejects() {
        let mut fx = clone_fixture("CC-20");
        // Single label span covers whole source with header+rewritten body.
        fx["candidate"]["derivation"] = serde_json::json!([
            {
                "kind": "label",
                "source_provider": "provider_a",
                "source_text": "goal fix the flaky auth test",
                "output_text": "Goal:\nPlease invent a rewritten body here.",
                "conversion_id": null,
                "label": "Goal"
            }
        ]);
        let got = run_fx(fx);
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert_eq!(
            got.error_codes(),
            &[ComposeErrorCode::InvalidLabel],
            "fat label must reject with exact InvalidLabel, codes={:?}",
            got.error_codes()
        );
        // Green path still: Goal:\n + keep body (corpus CC-20).
        let green = run_fx(clone_fixture("CC-20"));
        assert_eq!(green.decision(), CompositionDecision::Accept);
        assert_eq!(green.rendered(), "Goal:\nFix the flaky auth test.");
    }

    #[test]
    fn mutation_uncertain_backtrack_preserves_words() {
        // Mutate clear-backtrack accept fixture (CC-04) → uncertain.
        let mut fx = clone_fixture("CC-04");
        for rem in fx["candidate"]["removals"].as_array_mut().unwrap() {
            rem["certainty"] = Value::String("uncertain".into());
        }
        let got = run_fx(fx);
        assert_eq!(got.decision(), CompositionDecision::AcceptPreserveWords);
        assert_eq!(
            got.fallback_trigger(),
            Some(FallbackTrigger::UncertainBacktracking)
        );
        assert_eq!(got.error_codes(), &[ComposeErrorCode::UncertainBacktrack]);
        // Soft path renders host local baseline (preserve-words salvage).
        assert_eq!(got.rendered(), "Send it Monday.");
    }

    #[test]
    fn mutation_remove_without_declared_removals_rejects() {
        let mut fx = clone_fixture("CC-01");
        fx["candidate"]["removals"] = Value::Array(vec![]);
        fx["candidate"]["derivation"] = serde_json::json!([
            {
                "kind": "remove",
                "source_provider": "provider_a",
                "source_text": "ship",
                "output_text": "",
                "conversion_id": null,
                "label": null
            },
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "it exclamation point",
                "output_text": "It!",
                "conversion_id": null,
                "label": null
            }
        ]);
        let got = run_fx(fx);
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert!(got
            .error_codes()
            .contains(&ComposeErrorCode::Unverifiable));
    }

    #[test]
    fn mutation_unknown_conversion_rejects() {
        let mut fx = clone_fixture("CC-01");
        fx["candidate"]["conversions"][0]["id"] = Value::String("hey→Restart".into());
        let got = run_fx(fx);
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert!(got
            .error_codes()
            .contains(&ComposeErrorCode::UnknownConversion));
    }

    #[test]
    fn mutation_stale_fingerprint_rejects() {
        let mut fx = clone_fixture("CC-01");
        fx["candidate"]["base_fingerprint"] =
            Value::String(format!("sha256:{}", "a".repeat(64)));
        let got = run_fx(fx);
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert!(got.error_codes().contains(&ComposeErrorCode::Stale));
    }

    #[test]
    fn mutation_clear_natural_multiparagraph_rejects() {
        let mut fx = clone_fixture("CC-18");
        fx["candidate"]["layout"] = serde_json::json!({"decision": "natural", "certainty": "clear"});
        fx["candidate"]["derivation"] = serde_json::json!([
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hey can you send the notes",
                "output_text": "Hey, can you send the notes",
                "conversion_id": null,
                "label": null
            },
            {
                "kind": "layout_break",
                "source_provider": null,
                "source_text": "",
                "output_text": "\n\n",
                "conversion_id": null,
                "label": null
            },
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "when you get a chance",
                "output_text": "when you get a chance?",
                "conversion_id": null,
                "label": null
            }
        ]);
        let got = run_fx(fx);
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert!(got
            .error_codes()
            .contains(&ComposeErrorCode::UnsafeSemantics));
    }

    /// Type-level / API invariant: the sole public compose entry takes a
    /// structured candidate, never free-form model final prose.
    ///
    /// Documented entry points:
    /// - [`compose_structured_candidate`]
    /// - [`parse_structured_candidate_json`]
    /// - [`StructuredCandidate`] (no `final` / free-form field)
    /// - [`ComposeInput::local_baseline`] is [`LocalBaseline`], not `&str`
    #[test]
    fn invariant_no_public_raw_prose_accept_path() {
        // StructuredCandidate has no final/prose field — only derivation spans.
        let json = br#"{
            "schema_version": "1",
            "base_fingerprint": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "reconciliation": {"selected_provider": "provider_a", "reason": "only_available"},
            "removals": [],
            "conversions": [],
            "layout": {"decision": "natural", "certainty": "clear"},
            "labels": [],
            "derivation": [{
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hi",
                "output_text": "Hi",
                "conversion_id": null,
                "label": null
            }]
        }"#;
        let cand = parse_structured_candidate_json(json).expect("parse");
        // Confirm we cannot construct an accept path from raw prose alone:
        // without a structured candidate that passes gates, free-form strings
        // only appear sealed as host-supplied LocalBaseline.
        let sources = [ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: "hi".into(),
            primary: true,
        }];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "only_available".into(),
        };
        let baseline = LocalBaseline::from_organized_text("Hi.");
        let input = ComposeInput {
            local_baseline: &baseline,
            base_fingerprint: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            sources: &sources,
            source_selection: &selection,
            protected_tokens: &[],
            policy: RenderingPolicy::Adaptive,
            cloud_outcome: CloudOutcome::Succeeded,
            candidate: Some(&cand),
        };
        let _ = compose_structured_candidate(&input);
        // API surface: ComposeInput requires &LocalBaseline (not free &str).
        let _: fn(&ComposeInput<'_>) -> ComposeOutcome = compose_structured_candidate;
        let _: &LocalBaseline = input.local_baseline;
        assert_eq!(
            input.local_baseline.contract(),
            crate::local_baseline::LOCAL_BASELINE_CONTRACT_ID
        );
        assert!(std::mem::size_of_val(&cand.derivation) > 0);
    }

    #[test]
    fn compose_input_requires_local_baseline_type() {
        let baseline = LocalBaseline::from_organized_text("organized host text");
        assert_eq!(baseline.rendered(), "organized host text");
        assert_eq!(
            baseline.contract(),
            crate::local_baseline::LOCAL_BASELINE_CONTRACT_ID
        );
        let sources = [ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: "hi".into(),
            primary: true,
        }];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "only_available".into(),
        };
        let input = ComposeInput {
            local_baseline: &baseline,
            base_fingerprint: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            sources: &sources,
            source_selection: &selection,
            protected_tokens: &[],
            policy: RenderingPolicy::Adaptive,
            cloud_outcome: CloudOutcome::Skipped,
            candidate: None,
        };
        let got = compose_structured_candidate(&input);
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert_eq!(got.rendered(), "organized host text");
    }

    #[test]
    fn parse_rejects_unknown_final_field() {
        let json = br#"{
            "schema_version": "1",
            "base_fingerprint": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "reconciliation": {"selected_provider": "provider_a", "reason": "only_available"},
            "removals": [],
            "conversions": [],
            "layout": {"decision": "natural", "certainty": "clear"},
            "labels": [],
            "derivation": [{
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hi",
                "output_text": "Hi",
                "conversion_id": null,
                "label": null
            }],
            "final": "smuggled model prose"
        }"#;
        assert!(parse_structured_candidate_json(json).is_none());
    }

    #[test]
    fn parse_rejects_missing_required_key() {
        // Missing required `output_text` on derivation span.
        let json = br#"{
            "schema_version": "1",
            "base_fingerprint": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "reconciliation": {"selected_provider": "provider_a", "reason": "only_available"},
            "removals": [],
            "conversions": [],
            "layout": {"decision": "natural", "certainty": "clear"},
            "labels": [],
            "derivation": [{
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hi",
                "conversion_id": null,
                "label": null
            }]
        }"#;
        assert!(parse_structured_candidate_json(json).is_none());
    }

    #[test]
    fn parse_rejects_missing_nullable_conversion_id_key() {
        // Schema-required nullable: key must be present (null OK). Omitting the
        // key entirely is rejected even though Option would serde to None.
        let missing = br#"{
            "schema_version": "1",
            "base_fingerprint": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "reconciliation": {"selected_provider": "provider_a", "reason": "only_available"},
            "removals": [],
            "conversions": [],
            "layout": {"decision": "natural", "certainty": "clear"},
            "labels": [],
            "derivation": [{
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hi",
                "output_text": "Hi",
                "label": null
            }]
        }"#;
        assert!(parse_structured_candidate_json(missing).is_none());

        let present_null = br#"{
            "schema_version": "1",
            "base_fingerprint": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "reconciliation": {"selected_provider": "provider_a", "reason": "only_available"},
            "removals": [],
            "conversions": [],
            "layout": {"decision": "natural", "certainty": "clear"},
            "labels": [],
            "derivation": [{
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hi",
                "output_text": "Hi",
                "conversion_id": null,
                "label": null
            }]
        }"#;
        assert!(parse_structured_candidate_json(present_null).is_some());
    }

    #[test]
    fn parse_rejects_oversized_payload() {
        let mut raw = Vec::with_capacity(MAX_COMPOSE_CANDIDATE_BYTES + 8);
        raw.extend_from_slice(br#"{"schema_version":"1","pad":""#);
        while raw.len() <= MAX_COMPOSE_CANDIDATE_BYTES {
            raw.push(b'x');
        }
        raw.extend_from_slice(br#""}"#);
        assert!(raw.len() > MAX_COMPOSE_CANDIDATE_BYTES);
        assert!(parse_structured_candidate_json(&raw).is_none());
    }

    #[test]
    fn compose_rejects_oversize_in_memory_candidate() {
        // Bounds are rechecked at compose entry, not only at parse.
        let huge = "x".repeat(MAX_COMPOSE_FIELD_UTF8_BYTES + 1);
        let cand = StructuredCandidate {
            schema_version: "1".into(),
            base_fingerprint: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            reconciliation: Reconciliation {
                selected_provider: "provider_a".into(),
                reason: "only_available".into(),
            },
            removals: vec![],
            conversions: vec![],
            layout: LayoutClaim {
                decision: LayoutDecision::Natural,
                certainty: ComposeCertainty::Clear,
            },
            labels: vec![],
            derivation: vec![DerivationSpan {
                kind: SpanKind::Keep,
                source_provider: Some("provider_a".into()),
                source_text: huge.clone(),
                output_text: huge,
                conversion_id: None,
                label: None,
            }],
        };
        assert!(!candidate_within_bounds(&cand));
        let sources = [ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: "hi".into(),
            primary: true,
        }];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "only_available".into(),
        };
        let baseline = LocalBaseline::from_organized_text("Hi.");
        let input = ComposeInput {
            local_baseline: &baseline,
            base_fingerprint: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            sources: &sources,
            source_selection: &selection,
            protected_tokens: &[],
            policy: RenderingPolicy::Adaptive,
            cloud_outcome: CloudOutcome::Succeeded,
            candidate: Some(&cand),
        };
        let got = compose_structured_candidate(&input);
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert_eq!(
            got.fallback_trigger(),
            Some(FallbackTrigger::ResponseSchemaFailure)
        );
        assert!(got.error_codes().contains(&ComposeErrorCode::Schema));
        assert!(got.error_codes().contains(&ComposeErrorCode::Malformed));
        assert_eq!(got.rendered(), "Hi.");
    }

    #[test]
    fn find_literal_spans_unicode_accented_no_panic() {
        let hay = "call Émile about the release";
        let spans = find_literal_spans(hay, "Émile");
        assert_eq!(spans.len(), 1);
        let (s, e) = spans[0];
        assert!(hay.is_char_boundary(s) && hay.is_char_boundary(e));
        assert_eq!(&hay[s..e], "Émile");

        // Case-insensitive path must also stay on char boundaries.
        let spans_ci = find_literal_spans(hay, "émile");
        assert_eq!(spans_ci.len(), 1);
        let (s, e) = spans_ci[0];
        assert!(hay.is_char_boundary(s) && hay.is_char_boundary(e));
        assert_eq!(&hay[s..e], "Émile");

        // Combining mark sequence (e + combining acute) — fail closed or match
        // without panicking.
        let combining = "cafe\u{0301}"; // café via combining acute
        let _ = find_literal_spans(combining, "cafe\u{0301}");
        let _ = find_literal_spans(combining, "CAFÉ");
        // Overlapping / empty never panic
        assert!(find_literal_spans("", "x").is_empty());
        assert!(find_literal_spans("x", "").is_empty());
    }

    #[test]
    fn find_literal_spans_dotted_i_empty_atom_seq_no_panic() {
        // Sol repro: haystack "İ", needle "i\u{307}". After lowercase, norm
        // contains may match while haystack has zero ASCII regex atoms and
        // needle has non-empty atoms — must fail closed, never panic on empty seq.
        let hay = "İ";
        let needle = "i\u{307}";
        let spans = find_literal_spans(hay, needle);
        assert!(
            spans.is_empty(),
            "empty-atom haystack must not invent ranges, got {spans:?}"
        );
        // Empty-atom / punctuation-only edges never panic.
        let _ = find_literal_spans("İ İ", "i\u{307}");
        assert!(find_literal_spans("!!!", "i").is_empty());
    }

    #[test]
    fn find_literal_spans_mixed_case_both_occurrences() {
        // Exact-only early return used to drop case-folded second hit: source
        // `foo FOO` with needle `foo` must yield both non-overlapping ranges.
        let hay = "foo FOO";
        let spans = find_literal_spans(hay, "foo");
        assert_eq!(spans.len(), 2, "spans={spans:?}");
        assert_eq!(&hay[spans[0].0..spans[0].1], "foo");
        assert_eq!(&hay[spans[1].0..spans[1].1], "FOO");
        assert!(!ranges_overlap(spans[0], spans[1]));
    }

    #[test]
    fn claim_two_mixed_case_keeps_no_false_overlap() {
        // Two non-overlapping keep claims on `foo` / `FOO` both place.
        let mut fx = clone_fixture("CC-01");
        fx["sources"] = serde_json::json!([{
            "provider": "provider_a",
            "available": true,
            "text": "foo FOO",
            "primary": true
        }]);
        fx["base_fingerprint"] = Value::String(crate::text_sha256_fingerprint("foo FOO"));
        fx["candidate"]["base_fingerprint"] = fx["base_fingerprint"].clone();
        fx["candidate"]["removals"] = Value::Array(vec![]);
        fx["candidate"]["conversions"] = Value::Array(vec![]);
        fx["candidate"]["labels"] = Value::Array(vec![]);
        fx["candidate"]["derivation"] = serde_json::json!([
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "foo",
                "output_text": "foo ",
                "conversion_id": null,
                "label": null
            },
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "foo",
                "output_text": "FOO",
                "conversion_id": null,
                "label": null
            }
        ]);
        let got = run_fx(fx);
        assert_ne!(
            got.error_codes(),
            &[ComposeErrorCode::Overlap],
            "distinct mixed-case ranges must not false-overlap, codes={:?}",
            got.error_codes()
        );
        // Accept if other gates pass; at minimum no Overlap.
        assert!(!got.error_codes().contains(&ComposeErrorCode::Overlap));
    }

    #[test]
    fn find_literal_spans_no_mid_codepoint_advance_panic() {
        // Regression: advancing start by +1 byte after a multi-byte match used
        // to panic on the lowercased haystack slice.
        let hay = "Émile Émile";
        let spans = find_literal_spans(hay, "Émile");
        assert_eq!(spans.len(), 2);
        for (s, e) in spans {
            assert!(hay.is_char_boundary(s) && hay.is_char_boundary(e));
            assert_eq!(&hay[s..e], "Émile");
        }
    }

    #[test]
    fn unknown_label_wins_over_unknown_conversion() {
        let mut fx = clone_fixture("CC-01");
        fx["candidate"]["conversions"][0]["id"] = Value::String("nope→X".into());
        fx["candidate"]["labels"] = serde_json::json!([{
            "label": "NotAClosedLabel",
            "source_provider": "provider_a",
            "source_span_text": "ship"
        }]);
        // Shape validation runs on typed candidate — re-parse after mutation.
        let (owned, candidate) = fixture_input(&fx);
        // Deserialize succeeds; validate_candidate_shape rejects (label wins).
        let got = compose_owned(&owned, candidate.as_ref());
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert!(
            got.error_codes().contains(&ComposeErrorCode::UnknownLabel),
            "label should win last-write, codes={:?}",
            got.error_codes()
        );
        assert!(!got.error_codes().contains(&ComposeErrorCode::UnknownConversion));
    }

    #[test]
    fn delivery_flags_always_unsent() {
        let fx = clone_fixture("CC-01");
        let got = run_fx(fx);
        let d = got.delivery();
        assert_eq!(d.state, "unsent");
        assert!(!d.auto_send);
        assert!(!d.live_type);
        assert!(!d.replace_delivered);
    }

    #[test]
    fn skipped_cloud_returns_baseline_without_errors() {
        let fx = clone_fixture("CC-23");
        let got = run_fx(fx);
        assert_eq!(got.decision(), CompositionDecision::FallbackBaseline);
        assert!(got.fallback_trigger().is_none());
        assert!(got.error_codes().is_empty());
    }
}
