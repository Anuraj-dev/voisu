//! Bounded, redacted Developer Prompt Rendering diagnostics (DPR-T6 / #161).
//!
//! Production records only closed enums, relative timing, and fixed Delivery
//! evidence. Candidate text can exist only in the explicitly non-default
//! `dpr-eval-late-retain` build feature and is never connected to Delivery.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{
    CloudRequest, ComposeErrorCode, CompositionDecision, DeliveryFlags, FallbackTrigger,
    RenderingRoute, RoutingDecision, RuleId, DELIVERY_AUTO_SEND, DELIVERY_LIVE_TYPE,
    DELIVERY_REPLACE_DELIVERED, DELIVERY_STATE_UNSENT,
};

#[cfg(feature = "dpr-eval-late-retain")]
use crate::{
    clamp_utf8_bytes, is_text_sha256_fingerprint, scrub_embedded_urls, scrub_secret_values,
    text_sha256_fingerprint, ComposeOutcome,
};

/// Schema version for the persisted DPR diagnostic surface.
pub const DPR_DIAGNOSTIC_VERSION: u32 = 1;
/// Hard event cap inherited from the accepted #142 diagnostics package.
pub const MAX_DPR_DIAGNOSTIC_EVENTS: usize = 24;
/// The only optional user-facing status for a hard fallback after cloud began.
pub const DPR_LOCAL_FALLBACK_MESSAGE: &str = "Local formatting used";
/// Evaluation-only retained candidate text cap from #142.
#[cfg(feature = "dpr-eval-late-retain")]
pub const MAX_DPR_RETAINED_LATE_TEXT_UTF8_BYTES: usize = 2_048;

/// Production is the only mode compiled into default and release builds.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DprDiagnosticMode {
    Production,
    #[cfg(feature = "dpr-eval-late-retain")]
    Evaluation,
}

/// Closed event vocabulary. The evaluation retain event is absent from default
/// builds; production can retain timing-only discard evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DprDiagnosticEventName {
    RouteSelected,
    CloudSkipped,
    CloudRequestStarted,
    CloudResponseReceived,
    CloudDeadlineExceeded,
    ProviderFailed,
    SchemaValidationFailed,
    SourceDerivationFailed,
    CompositionAccepted,
    FallbackBaselineSelected,
    DeliveryEmitted,
    #[cfg(feature = "dpr-eval-late-retain")]
    LateResultRetained,
    LateResultDiscarded,
}

/// One event measured from `utterance_end`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DprDiagnosticEvent {
    name: DprDiagnosticEventName,
    t_ms: u64,
}

impl DprDiagnosticEvent {
    #[must_use]
    pub const fn name(&self) -> DprDiagnosticEventName {
        self.name
    }

    #[must_use]
    pub const fn t_ms(&self) -> u64 {
        self.t_ms
    }
}

/// User feedback is deliberately a two-value closed catalog.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DprFeedbackKind {
    Silent,
    MinimalStatus,
}

/// Fixed Delivery evidence copied from the sole compose accept/fallback path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DprDeliveryEvidence {
    state: String,
    auto_send: bool,
    live_type: bool,
    replace_delivered: bool,
}

impl DprDeliveryEvidence {
    fn from_flags(flags: DeliveryFlags) -> Self {
        Self {
            state: flags.state.to_owned(),
            auto_send: flags.auto_send,
            live_type: flags.live_type,
            replace_delivered: flags.replace_delivered,
        }
    }

    fn dpr_default() -> Self {
        Self {
            state: DELIVERY_STATE_UNSENT.to_owned(),
            auto_send: DELIVERY_AUTO_SEND,
            live_type: DELIVERY_LIVE_TYPE,
            replace_delivered: DELIVERY_REPLACE_DELIVERED,
        }
    }

    #[must_use]
    pub fn state(&self) -> &str {
        &self.state
    }

    #[must_use]
    pub const fn auto_send(&self) -> bool {
        self.auto_send
    }

    #[must_use]
    pub const fn live_type(&self) -> bool {
        self.live_type
    }

    #[must_use]
    pub const fn replace_delivered(&self) -> bool {
        self.replace_delivered
    }
}

/// Offline comparison evidence available only in an explicit evaluation build.
/// No method on this type or [`DprDiagnostic`] can perform Delivery.
#[cfg(feature = "dpr-eval-late-retain")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DprLateEvaluationRecord {
    pub(crate) arrived_t_ms: u64,
    pub(crate) candidate_fingerprint: String,
    pub(crate) candidate_text_clamped: String,
    pub(crate) would_have_decision: CompositionDecision,
    pub(crate) compare_to_delivered: bool,
}

/// Proof that the sole compose gate accepted a late candidate. Fields are
/// private so evaluation code cannot manufacture acceptance from raw model text
/// or a caller-supplied decision.
#[cfg(feature = "dpr-eval-late-retain")]
pub struct DprAcceptedLateCandidate {
    rendered: String,
    decision: CompositionDecision,
}

#[cfg(feature = "dpr-eval-late-retain")]
impl DprAcceptedLateCandidate {
    /// Converts only an actual accept/soft-salvage compose outcome into the
    /// capability required by the evaluation retention lane.
    #[must_use]
    pub fn from_compose(outcome: &ComposeOutcome) -> Option<Self> {
        match outcome.decision() {
            CompositionDecision::Accept
            | CompositionDecision::AcceptPreserveWords
            | CompositionDecision::AcceptNaturalLayout => Some(Self {
                rendered: outcome.rendered().to_owned(),
                decision: outcome.decision(),
            }),
            CompositionDecision::FallbackBaseline => None,
        }
    }
}

#[cfg(feature = "dpr-eval-late-retain")]
impl DprLateEvaluationRecord {
    #[must_use]
    pub const fn arrived_t_ms(&self) -> u64 {
        self.arrived_t_ms
    }

    #[must_use]
    pub fn candidate_fingerprint(&self) -> &str {
        &self.candidate_fingerprint
    }

    #[must_use]
    pub fn candidate_text_clamped(&self) -> &str {
        &self.candidate_text_clamped
    }

    #[must_use]
    pub const fn would_have_decision(&self) -> CompositionDecision {
        self.would_have_decision
    }

    #[must_use]
    pub const fn compare_to_delivered(&self) -> bool {
        self.compare_to_delivered
    }
}

/// One bounded production (or explicit evaluation-build) diagnostic for a
/// Recording. All timings are relative to `utterance_end`.
///
/// No build configuration exposes an API that applies a late result:
///
/// ```compile_fail
/// use voisu_core::apply_late_result;
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DprDiagnostic {
    version: u32,
    mode: DprDiagnosticMode,
    route: RenderingRoute,
    cloud_request: CloudRequest,
    rule_id: RuleId,
    events: Vec<DprDiagnosticEvent>,
    cloud_attempted: bool,
    compose_decision: Option<CompositionDecision>,
    fallback_trigger: Option<FallbackTrigger>,
    reason_codes: Vec<ComposeErrorCode>,
    feedback_kind: DprFeedbackKind,
    feedback_message: Option<String>,
    delivery: DprDeliveryEvidence,
    #[cfg(feature = "dpr-eval-late-retain")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) late_evaluation: Option<DprLateEvaluationRecord>,
}

impl DprDiagnostic {
    /// Starts the default production timeline with `route_selected`.
    #[must_use]
    pub fn production(routing: &RoutingDecision, at: Duration) -> Self {
        Self::new(DprDiagnosticMode::Production, routing, at)
    }

    #[cfg(feature = "dpr-eval-late-retain")]
    #[must_use]
    pub fn evaluation(routing: &RoutingDecision, at: Duration) -> Self {
        Self::new(DprDiagnosticMode::Evaluation, routing, at)
    }

    fn new(mode: DprDiagnosticMode, routing: &RoutingDecision, at: Duration) -> Self {
        let mut diagnostic = Self {
            version: DPR_DIAGNOSTIC_VERSION,
            mode,
            route: routing.route,
            cloud_request: routing.cloud_request,
            rule_id: routing.rule_id,
            events: Vec::new(),
            cloud_attempted: false,
            compose_decision: None,
            fallback_trigger: None,
            reason_codes: Vec::new(),
            feedback_kind: DprFeedbackKind::Silent,
            feedback_message: None,
            delivery: DprDeliveryEvidence::dpr_default(),
            #[cfg(feature = "dpr-eval-late-retain")]
            late_evaluation: None,
        };
        diagnostic.push_event(DprDiagnosticEventName::RouteSelected, at);
        diagnostic
    }

    pub fn cloud_skipped(&mut self, at: Duration) {
        self.push_event(DprDiagnosticEventName::CloudSkipped, at);
    }

    pub fn cloud_request_started(&mut self, at: Duration) {
        self.cloud_attempted = true;
        self.push_event(DprDiagnosticEventName::CloudRequestStarted, at);
    }

    pub fn cloud_response_received(&mut self, at: Duration) {
        if !self.cloud_attempted {
            return;
        }
        self.push_event(DprDiagnosticEventName::CloudResponseReceived, at);
    }

    /// Records the closed compose result. This method emits the terminal cloud
    /// failure class, if any, before the accept/fallback decision.
    pub fn composition_completed(
        &mut self,
        decision: CompositionDecision,
        fallback_trigger: Option<FallbackTrigger>,
        reason_codes: &[ComposeErrorCode],
        at: Duration,
    ) {
        if self.compose_decision.is_some() {
            return;
        }
        self.compose_decision = Some(decision);
        self.fallback_trigger = fallback_trigger;
        self.reason_codes.clear();
        self.reason_codes.extend(
            reason_codes
                .iter()
                .copied()
                .take(MAX_DPR_DIAGNOSTIC_EVENTS),
        );

        if self.cloud_attempted {
            match fallback_trigger {
                Some(FallbackTrigger::DeadlineExceeded) => {
                    self.push_event(DprDiagnosticEventName::CloudDeadlineExceeded, at);
                }
                Some(FallbackTrigger::ProviderFailure) => {
                    self.push_event(DprDiagnosticEventName::ProviderFailed, at);
                }
                Some(FallbackTrigger::ResponseSchemaFailure) => {
                    self.push_event(DprDiagnosticEventName::SchemaValidationFailed, at);
                }
                Some(
                    FallbackTrigger::UnsafeSemantics
                    | FallbackTrigger::UnverifiableSourceDerivation
                    | FallbackTrigger::InvalidFixedLabel,
                ) => {
                    self.push_event(DprDiagnosticEventName::SourceDerivationFailed, at);
                }
                Some(FallbackTrigger::UncertainBacktracking | FallbackTrigger::UncertainLayout)
                | None => {}
            }
        }

        match decision {
            CompositionDecision::Accept
            | CompositionDecision::AcceptPreserveWords
            | CompositionDecision::AcceptNaturalLayout => {
                self.push_event(DprDiagnosticEventName::CompositionAccepted, at);
                self.feedback_kind = DprFeedbackKind::Silent;
                self.feedback_message = None;
            }
            CompositionDecision::FallbackBaseline => {
                self.push_event(DprDiagnosticEventName::FallbackBaselineSelected, at);
                if self.cloud_attempted && fallback_trigger.is_some() {
                    self.feedback_kind = DprFeedbackKind::MinimalStatus;
                    self.feedback_message = Some(DPR_LOCAL_FALLBACK_MESSAGE.to_owned());
                } else {
                    self.feedback_kind = DprFeedbackKind::Silent;
                    self.feedback_message = None;
                }
            }
        }
    }

    pub fn delivery_emitted(&mut self, at: Duration, flags: DeliveryFlags) {
        if self.compose_decision.is_none()
            || self
                .events
                .iter()
                .any(|event| event.name == DprDiagnosticEventName::DeliveryEmitted)
        {
            return;
        }
        self.delivery = DprDeliveryEvidence::from_flags(flags);
        self.push_event(DprDiagnosticEventName::DeliveryEmitted, at);
    }

    /// Production late evidence intentionally accepts timing only.
    pub fn late_result_discarded(&mut self, at: Duration) {
        if !self
            .events
            .iter()
            .any(|event| event.name == DprDiagnosticEventName::DeliveryEmitted)
        {
            return;
        }
        self.push_event(DprDiagnosticEventName::LateResultDiscarded, at);
    }

    /// Retains one clamped late candidate for offline comparison only in an
    /// evaluation-mode binary. This never invokes or mutates Delivery.
    #[cfg(feature = "dpr-eval-late-retain")]
    pub fn retain_late_candidate_for_compare(
        &mut self,
        at: Duration,
        candidate: &DprAcceptedLateCandidate,
        sensitive_values: &[String],
    ) -> bool {
        if self.mode != DprDiagnosticMode::Evaluation
            || self.late_evaluation.is_some()
            || !self
                .events
                .iter()
                .any(|event| event.name == DprDiagnosticEventName::DeliveryEmitted)
        {
            return false;
        }
        let arrived_t_ms = duration_ms(at);
        let redacted = scrub_embedded_urls(&scrub_secret_values(
            &candidate.rendered,
            sensitive_values,
        ));
        self.late_evaluation = Some(DprLateEvaluationRecord {
            arrived_t_ms,
            candidate_fingerprint: text_sha256_fingerprint(&candidate.rendered),
            candidate_text_clamped: clamp_utf8_bytes(
                &redacted,
                MAX_DPR_RETAINED_LATE_TEXT_UTF8_BYTES,
            ),
            would_have_decision: candidate.decision,
            compare_to_delivered: true,
        });
        self.push_event(DprDiagnosticEventName::LateResultRetained, at);
        true
    }

    fn push_event(&mut self, name: DprDiagnosticEventName, at: Duration) {
        if self.events.len() >= MAX_DPR_DIAGNOSTIC_EVENTS
            || self.events.iter().any(|event| event.name == name)
        {
            return;
        }
        let previous = self.events.last().map_or(0, |event| event.t_ms);
        self.events.push(DprDiagnosticEvent {
            name,
            t_ms: duration_ms(at).max(previous),
        });
    }

    #[must_use]
    pub const fn mode(&self) -> DprDiagnosticMode {
        self.mode
    }

    #[must_use]
    pub fn events(&self) -> &[DprDiagnosticEvent] {
        &self.events
    }

    #[must_use]
    pub const fn feedback_kind(&self) -> DprFeedbackKind {
        self.feedback_kind
    }

    #[must_use]
    pub fn feedback_message(&self) -> Option<&str> {
        self.feedback_message.as_deref()
    }

    #[must_use]
    pub fn reason_codes(&self) -> &[ComposeErrorCode] {
        &self.reason_codes
    }

    #[must_use]
    pub const fn delivery_flags(&self) -> &DprDeliveryEvidence {
        &self.delivery
    }

    #[cfg(feature = "dpr-eval-late-retain")]
    #[must_use]
    pub fn late_evaluation(&self) -> Option<&DprLateEvaluationRecord> {
        self.late_evaluation.as_ref()
    }

    pub(crate) fn normalize(&mut self) {
        self.version = DPR_DIAGNOSTIC_VERSION;
        self.events.truncate(MAX_DPR_DIAGNOSTIC_EVENTS);
        let mut previous = 0;
        self.events.retain(|event| {
            let keep = event.t_ms >= previous;
            if keep {
                previous = event.t_ms;
            }
            keep
        });
        self.reason_codes.truncate(MAX_DPR_DIAGNOSTIC_EVENTS);
        match self.feedback_kind {
            DprFeedbackKind::Silent => self.feedback_message = None,
            DprFeedbackKind::MinimalStatus => {
                self.feedback_message = Some(DPR_LOCAL_FALLBACK_MESSAGE.to_owned());
            }
        }
        self.delivery = DprDeliveryEvidence::dpr_default();
        #[cfg(feature = "dpr-eval-late-retain")]
        if let Some(late) = self.late_evaluation.as_mut() {
            late.candidate_text_clamped = clamp_utf8_bytes(
                &late.candidate_text_clamped,
                MAX_DPR_RETAINED_LATE_TEXT_UTF8_BYTES,
            );
            if !is_text_sha256_fingerprint(&late.candidate_fingerprint) {
                late.candidate_fingerprint =
                    text_sha256_fingerprint(&late.candidate_text_clamped);
            }
            late.compare_to_delivered = true;
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Default builds have no evaluation late-copy symbol.
///
/// ```compile_fail
/// use voisu_core::DprLateEvaluationRecord;
/// ```
#[cfg(not(feature = "dpr-eval-late-retain"))]
pub const DPR_EVALUATION_LANE_COMPILE_GATED: () = ();
