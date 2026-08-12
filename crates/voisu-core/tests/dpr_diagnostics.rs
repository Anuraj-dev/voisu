use std::time::Duration;

use voisu_core::{
    CloudRequest, ComposeErrorCode, CompositionDecision, DeliveryFlags, DprDiagnostic,
    DprDiagnosticEventName, DprDiagnosticMode, DprFeedbackKind, FallbackTrigger, RenderingRoute,
    RoutingDecision, RuleId, DPR_LOCAL_FALLBACK_MESSAGE, MAX_DPR_DIAGNOSTIC_EVENTS,
};

fn cloud_route() -> RoutingDecision {
    RoutingDecision {
        route: RenderingRoute::LocalWithOptionalCloud,
        cloud_request: CloudRequest::Allowed,
        rule_id: RuleId::ComplexCloud,
        complexity_score: 7,
        contributions: Vec::new(),
        surface_degraded: false,
        section_cue_count: 0,
    }
}

#[test]
fn production_diagnostic_records_a_bounded_utterance_end_timeline_and_hard_fallback_feedback() {
    let mut diagnostic = DprDiagnostic::production(&cloud_route(), Duration::from_millis(5));
    diagnostic.cloud_request_started(Duration::from_millis(10));
    diagnostic.cloud_response_received(Duration::from_millis(70));
    diagnostic.composition_completed(
        CompositionDecision::FallbackBaseline,
        Some(FallbackTrigger::UnverifiableSourceDerivation),
        &[ComposeErrorCode::Unverifiable],
        Duration::from_millis(75),
    );
    diagnostic.delivery_emitted(Duration::from_millis(80), DeliveryFlags::dpr_default());

    assert_eq!(diagnostic.mode(), DprDiagnosticMode::Production);
    assert_eq!(diagnostic.feedback_kind(), DprFeedbackKind::MinimalStatus);
    assert_eq!(
        diagnostic.feedback_message(),
        Some(DPR_LOCAL_FALLBACK_MESSAGE)
    );
    assert_eq!(
        diagnostic
            .events()
            .iter()
            .map(|event| event.name())
            .collect::<Vec<_>>(),
        vec![
            DprDiagnosticEventName::RouteSelected,
            DprDiagnosticEventName::CloudRequestStarted,
            DprDiagnosticEventName::CloudResponseReceived,
            DprDiagnosticEventName::SourceDerivationFailed,
            DprDiagnosticEventName::FallbackBaselineSelected,
            DprDiagnosticEventName::DeliveryEmitted,
        ]
    );
    assert!(diagnostic
        .events()
        .windows(2)
        .all(|events| events[0].t_ms() <= events[1].t_ms()));
    assert!(diagnostic.events().len() <= MAX_DPR_DIAGNOSTIC_EVENTS);
    assert_eq!(diagnostic.reason_codes(), &[ComposeErrorCode::Unverifiable]);
    assert!(!diagnostic.delivery_flags().replace_delivered());
}

#[test]
fn production_late_evidence_is_timing_only_and_cannot_replace_delivery() {
    let mut diagnostic = DprDiagnostic::production(&cloud_route(), Duration::ZERO);
    diagnostic.cloud_request_started(Duration::from_millis(5));
    diagnostic.composition_completed(
        CompositionDecision::FallbackBaseline,
        Some(FallbackTrigger::DeadlineExceeded),
        &[ComposeErrorCode::Deadline],
        Duration::from_millis(1_500),
    );
    diagnostic.delivery_emitted(
        Duration::from_millis(1_500),
        DeliveryFlags::dpr_default(),
    );
    diagnostic.late_result_discarded(Duration::from_millis(1_675));

    let encoded = serde_json::to_string(&diagnostic).expect("serialize production diagnostic");
    assert!(encoded.contains("late_result_discarded"));
    assert!(!encoded.contains("late_result_retained"));
    assert!(!encoded.contains("candidate_text"));
    assert!(!encoded.contains("apply_late"));
    assert!(!encoded.contains("replace_delivered\":true"));
    assert_eq!(diagnostic.events().last().expect("late event").t_ms(), 1_675);
}

#[test]
fn feedback_is_silent_without_an_attempt_or_when_composition_accepts() {
    let mut skipped = DprDiagnostic::production(&cloud_route(), Duration::ZERO);
    skipped.cloud_skipped(Duration::from_millis(1));
    skipped.composition_completed(
        CompositionDecision::FallbackBaseline,
        None,
        &[],
        Duration::from_millis(2),
    );
    skipped.delivery_emitted(Duration::from_millis(3), DeliveryFlags::dpr_default());
    assert_eq!(skipped.feedback_kind(), DprFeedbackKind::Silent);
    assert_eq!(skipped.feedback_message(), None);

    let mut accepted = DprDiagnostic::production(&cloud_route(), Duration::ZERO);
    accepted.cloud_request_started(Duration::from_millis(1));
    accepted.cloud_response_received(Duration::from_millis(2));
    accepted.composition_completed(
        CompositionDecision::Accept,
        None,
        &[],
        Duration::from_millis(3),
    );
    accepted.delivery_emitted(Duration::from_millis(4), DeliveryFlags::dpr_default());
    assert_eq!(accepted.feedback_kind(), DprFeedbackKind::Silent);
    assert_eq!(accepted.feedback_message(), None);
}

#[cfg(feature = "dpr-eval-late-retain")]
#[test]
fn evaluation_feature_retains_one_clamped_candidate_for_compare_without_delivery_mutation() {
    use voisu_core::{
        export_record, DiagnosticRecord, MAX_DPR_RETAINED_LATE_TEXT_UTF8_BYTES, REDACTED,
    };

    let mut evaluation = DprDiagnostic::evaluation(&cloud_route(), Duration::ZERO);
    evaluation.cloud_request_started(Duration::from_millis(1));
    evaluation.composition_completed(
        CompositionDecision::FallbackBaseline,
        Some(FallbackTrigger::DeadlineExceeded),
        &[ComposeErrorCode::Deadline],
        Duration::from_millis(1_500),
    );
    evaluation.delivery_emitted(
        Duration::from_millis(1_500),
        DeliveryFlags::dpr_default(),
    );

    let secret = "eval-secret";
    let candidate = format!(
        "{secret}{}",
        "x".repeat(MAX_DPR_RETAINED_LATE_TEXT_UTF8_BYTES + 64)
    );
    assert!(evaluation.retain_late_candidate_for_compare(
        Duration::from_millis(1_675),
        &candidate,
        CompositionDecision::Accept,
    ));
    assert!(!evaluation.retain_late_candidate_for_compare(
        Duration::from_millis(1_700),
        "second candidate",
        CompositionDecision::Accept,
    ));

    let late = evaluation.late_evaluation().expect("evaluation record");
    assert_eq!(late.arrived_t_ms(), 1_675);
    assert_eq!(
        late.candidate_text_clamped().len(),
        MAX_DPR_RETAINED_LATE_TEXT_UTF8_BYTES
    );
    assert!(late.candidate_fingerprint().starts_with("sha256:"));
    assert!(late.compare_to_delivered());
    assert!(!evaluation.delivery_flags().replace_delivered());

    let mut record = DiagnosticRecord::new("rec-eval-redaction".to_owned(), 7);
    record.dpr = Some(evaluation);
    let exported = export_record(
        record,
        [("VOISU_GROQ_API_KEY".to_owned(), secret.to_owned())],
    );
    let encoded = serde_json::to_string(&exported).expect("serialize redacted export");
    assert!(!encoded.contains(secret));
    assert!(encoded.contains(REDACTED));

    let mut production = DprDiagnostic::production(&cloud_route(), Duration::ZERO);
    assert!(!production.retain_late_candidate_for_compare(
        Duration::from_millis(1_675),
        "must not be retained",
        CompositionDecision::Accept,
    ));
    assert!(production.late_evaluation().is_none());
}
