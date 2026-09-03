//! DPR-T7 hermetic ship gates from specification §15.
//!
//! Run the complete gate (including the full #138/#139 product corpus ports)
//! with `voisu-cargo test --workspace -- --test-threads=4`. This target uses no
//! real network, compositor, or wall-clock sleep.
//! Production dispatch/snapshot assertions share the existing hermetic daemon
//! harness in `daemon_cli_lifecycle`: `smart_writing_real_wiring_...` proves
//! flag-off routing, while `flagged_dpr_snapshots_policy_per_recording_...`
//! proves Natural is held in-flight and a later Recording observes Structured.

use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use voisu_app::config::WritingMode;
use voisu_app::dpr_cloud::{DprCloudAttempt, DprCloudErrorClass, DprCloudRequest};
use voisu_app::dpr_pipeline::{
    DprCloudBoundary, DprCloudCapability, DprCloudFuture, DprPipelineClock, DprTransformInput,
    dpr_source_context, dpr_transform_and_deliver,
};
use voisu_app::smart_writing::{
    CredentialGateEvidence, FinalTransformInput, GrammarGateCapability, ResolvedRecordingLanguages,
    final_transform_and_deliver,
};
use voisu_core::{
    BoundaryFuture, CloudOutcome, CloudRequest, ComposeInput, ComposeOutcome, ComposeSource,
    CompositionDecision, Credential, DeliveryAdapter, DeliveryFlags, DeliveryOutcome,
    DprDiagnosticMode, FormatEditCandidate, LocalBaseline, LocalBaselineOptions, LocalTiming,
    PauseBoundary, Provider, ProviderState, RenderingPolicy, RenderingRoute, SourceSelection,
    SourceTranscript, StructuredCandidate, SttProvider, SurfaceHint, TimingCertainty, Transcript,
    TranscriptSelection, compose_structured_candidate, organize_local_baseline,
    parse_format_edit_candidate_json, parse_structured_candidate_json,
};

const BEHAVIOR_CORPUS: &str = include_str!(
    "../../../docs/research/developer-prompt-rendering-behavior-corpus-2026-08-11.json"
);
const COMPOSE_CORPUS: &str = include_str!(
    "../../../docs/research/developer-prompt-rendering-combined-call-corpus-2026-08-11.json"
);

fn policy(value: &Value) -> RenderingPolicy {
    RenderingPolicy::parse(value.as_str().expect("policy string")).expect("known policy")
}

fn route(value: &Value) -> RenderingRoute {
    RenderingRoute::parse(value.as_str().expect("route string")).expect("known route")
}

fn stt_provider(value: &Value) -> SttProvider {
    match value.as_str().expect("provider string") {
        "provider_a" => SttProvider::ProviderA,
        "provider_b" => SttProvider::ProviderB,
        provider => panic!("unknown provider {provider}"),
    }
}

fn corpus_sources(fixture: &Value) -> Vec<SourceTranscript> {
    fixture["sources"]
        .as_array()
        .expect("sources")
        .iter()
        .filter(|source| source["available"] == true)
        .map(|source| SourceTranscript {
            provider: match source["provider"].as_str().expect("provider") {
                "provider_a" => Provider::Groq,
                "provider_b" => Provider::Deepgram,
                provider => panic!("unknown provider {provider}"),
            },
            text: source["text"].as_str().expect("source text").to_owned(),
        })
        .collect()
}

fn local_timing(fixture: &Value) -> Option<LocalTiming> {
    let timing = fixture.get("timing")?;
    if timing.is_null() {
        return None;
    }
    Some(LocalTiming {
        certainty: match timing["certainty"].as_str().expect("timing certainty") {
            "clear" => TimingCertainty::Clear,
            "uncertain" => TimingCertainty::Uncertain,
            certainty => panic!("unknown timing certainty {certainty}"),
        },
        boundaries: timing["boundaries"]
            .as_array()
            .expect("timing boundaries")
            .iter()
            .map(|boundary| PauseBoundary {
                left_phrase: boundary["left_phrase"]
                    .as_str()
                    .expect("left phrase")
                    .to_owned(),
                right_phrase: boundary["right_phrase"]
                    .as_str()
                    .expect("right phrase")
                    .to_owned(),
                pause_ms: u32::try_from(boundary["pause_ms"].as_u64().expect("pause ms"))
                    .expect("pause fits u32"),
            })
            .collect(),
    })
}

/// Ship gate 1 (#138): every fixture's deterministic local baseline, including
/// the former DPR-33 deferral and all optional-cloud fallback baselines, passes
/// through real source selection and the product organizer.
#[test]
fn behavior_corpus_all_48_local_baselines_match() {
    let corpus: Value = serde_json::from_str(BEHAVIOR_CORPUS).expect("behavior corpus");
    let mut checked = 0usize;
    for fixture in corpus["fixtures"].as_array().expect("fixtures") {
        let id = fixture["id"].as_str().expect("fixture id");
        let dictionary_terms = vec!["voisu".to_owned()];
        let context = dpr_source_context(&corpus_sources(fixture), &dictionary_terms)
            .unwrap_or_else(|| panic!("{id}: source context"));
        assert_eq!(
            context.source_selection.reason,
            fixture["source_selection"]["reason"]
                .as_str()
                .expect("reason"),
            "{id}: selection reason"
        );
        assert_eq!(
            context.provider_state.as_str(),
            fixture["provider_state"].as_str().expect("provider state"),
            "{id}: provider state"
        );
        let baseline = organize_local_baseline(
            &context.selected_source,
            &LocalBaselineOptions {
                policy: policy(&fixture["policy"]),
                route: route(&fixture["route"]),
                timing: local_timing(fixture),
            },
        );
        assert_eq!(
            baseline.rendered(),
            fixture["local_baseline"].as_str().expect("local baseline"),
            "{id}: local baseline"
        );
        assert_eq!(
            fixture["local_baseline"], fixture["expected_final"],
            "{id}: #138 v1.7 requires fallback/local final to equal its baseline"
        );
        assert_eq!(fixture["delivery"]["state"], "unsent", "{id}: state");
        assert!(
            !fixture["delivery"]["auto_send"]
                .as_bool()
                .expect("auto-send"),
            "{id}: auto-send"
        );
        assert!(
            !fixture["delivery"]["live_type"]
                .as_bool()
                .expect("live-type"),
            "{id}: live-type"
        );
        assert!(
            !fixture["delivery"]["replace_delivered"]
                .as_bool()
                .expect("replace-delivered"),
            "{id}: replace"
        );
        checked += 1;
    }
    assert_eq!(checked, 48, "full #138 local baseline corpus changed");
}

#[test]
fn silence_plus_pure_outro_selects_no_source_and_never_enters_transform() {
    // Deepgram empty + Groq "Thank you for watching!" must not become a
    // successful DPR Source Transcript (and therefore cannot cloud-call or
    // Deliver under the DPR branch).
    let sources = [
        SourceTranscript {
            provider: Provider::Deepgram,
            text: String::new(),
        },
        SourceTranscript {
            provider: Provider::Groq,
            text: "Thank you for watching!".to_owned(),
        },
    ];
    assert!(
        dpr_source_context(&sources, &[]).is_none(),
        "pure-outro silence must yield no DPR context"
    );
}

#[test]
fn complementary_merge_rejects_semantic_and_negation_gaps() {
    for (left, right, expected_state) in [
        (
            "deploy the service",
            "deploy the database",
            ProviderState::SemanticDisagreement,
        ),
        (
            "do not deploy",
            "do deploy now",
            ProviderState::SemanticDisagreement,
        ),
        (
            "call Anuraj tomorrow",
            "call Raja tomorrow",
            ProviderState::SemanticDisagreement,
        ),
    ] {
        let context = dpr_source_context(
            &[
                SourceTranscript {
                    provider: Provider::Groq,
                    text: left.to_owned(),
                },
                SourceTranscript {
                    provider: Provider::Deepgram,
                    text: right.to_owned(),
                },
            ],
            &[],
        )
        .expect("source context");
        assert_eq!(
            context.provider_state, expected_state,
            "unsafe merge for {left:?} / {right:?}"
        );
        assert_eq!(context.selected_source, left);
    }
}

#[test]
fn complementary_merge_is_order_independent_and_truthfully_attributed() {
    let groq = SourceTranscript {
        provider: Provider::Groq,
        text: "open crates/voisu-core/src/lib.rs and check".to_owned(),
    };
    let deepgram = SourceTranscript {
        provider: Provider::Deepgram,
        text: "open and check correlation_id".to_owned(),
    };
    let forward = dpr_source_context(&[groq.clone(), deepgram.clone()], &[]).expect("forward");
    let reversed = dpr_source_context(&[deepgram, groq], &[]).expect("reversed");
    for context in [&forward, &reversed] {
        assert_eq!(
            context.selected_source,
            "open crates/voisu-core/src/lib.rs and check correlation_id"
        );
        assert_eq!(context.provider_state, ProviderState::SafeComplementary);
        assert_eq!(
            context.transcript_selection,
            TranscriptSelection::Complementary
        );
        assert_eq!(context.sources[0].provider, SttProvider::ProviderA);
        assert!(context.sources[0].primary);
    }
    assert_eq!(forward.selected_source, reversed.selected_source);
}

struct ComposeFixture {
    baseline: LocalBaseline,
    fingerprint: String,
    sources: Vec<ComposeSource>,
    selection: SourceSelection,
    protected: Vec<String>,
    policy: RenderingPolicy,
    cloud: CloudOutcome,
    candidate: Option<StructuredCandidate>,
}

fn compose_fixture(fixture: &Value) -> ComposeFixture {
    let sources: Vec<ComposeSource> = fixture["sources"]
        .as_array()
        .expect("compose sources")
        .iter()
        .map(|source| ComposeSource {
            provider: stt_provider(&source["provider"]),
            available: source["available"].as_bool().expect("available"),
            text: source["text"].as_str().unwrap_or_default().to_owned(),
            primary: source["primary"].as_bool().expect("primary"),
        })
        .collect();
    let selected_provider = stt_provider(&fixture["source_selection"]["selected_provider"]);
    let source = sources
        .iter()
        .find(|source| source.provider == selected_provider && source.available)
        .expect("selected source");
    let policy = policy(&fixture["policy"]);
    let baseline = organize_local_baseline(
        &source.text,
        &LocalBaselineOptions {
            policy,
            route: RenderingRoute::LocalWithOptionalCloud,
            timing: None,
        },
    );
    let candidate = if fixture["candidate"].is_null() {
        None
    } else {
        Some(
            parse_structured_candidate_json(
                &serde_json::to_vec(&fixture["candidate"]).expect("candidate JSON"),
            )
            .unwrap_or_else(|| panic!("{}: candidate rejected", fixture["id"])),
        )
    };
    ComposeFixture {
        baseline,
        fingerprint: fixture["base_fingerprint"]
            .as_str()
            .expect("fingerprint")
            .to_owned(),
        sources,
        selection: SourceSelection {
            selected_provider,
            reason: fixture["source_selection"]["reason"]
                .as_str()
                .expect("reason")
                .to_owned(),
        },
        protected: fixture["protected_tokens"]
            .as_array()
            .expect("protected")
            .iter()
            .map(|token| token.as_str().expect("protected token").to_owned())
            .collect(),
        policy,
        cloud: CloudOutcome::parse(fixture["cloud_outcome"].as_str().expect("cloud outcome"))
            .expect("known cloud outcome"),
        candidate,
    }
}

fn compose(fixture: &Value) -> ComposeOutcome {
    let fixture = compose_fixture(fixture);
    let protected: Vec<&str> = fixture.protected.iter().map(String::as_str).collect();
    compose_structured_candidate(&ComposeInput {
        local_baseline: &fixture.baseline,
        base_fingerprint: &fixture.fingerprint,
        sources: &fixture.sources,
        source_selection: &fixture.selection,
        protected_tokens: &protected,
        policy: fixture.policy,
        cloud_outcome: fixture.cloud,
        candidate: fixture.candidate.as_ref(),
    })
}

fn corpus_fixture(corpus: &Value, id: &str) -> Value {
    corpus["fixtures"]
        .as_array()
        .expect("fixtures")
        .iter()
        .find(|fixture| fixture["id"] == id)
        .unwrap_or_else(|| panic!("missing fixture {id}"))
        .clone()
}

fn rejected_mutation(corpus: &Value, id: &str, name: &str, mutate: impl FnOnce(&mut Value)) {
    let mut fixture = corpus_fixture(corpus, id);
    mutate(&mut fixture);
    let outcome = compose(&fixture);
    assert_eq!(
        outcome.decision(),
        CompositionDecision::FallbackBaseline,
        "#139 mutation {name} accepted: {:?}",
        outcome.error_codes()
    );
    assert_eq!(
        outcome.delivery(),
        DeliveryFlags::dpr_default(),
        "#139 mutation {name} delivery flags"
    );
}

/// Ship gates 1/3/4 (#139): all 24 normative decisions/error codes, accepted
/// renders, and the 19 product-compose mutations from the v1.1.2 oracle stay
/// fail-closed. Under specification §14 precedence, fallback renders are
/// asserted against the current #138 Final Transcript authority; #139 owns the
/// structured compose decision rather than an independently rebuilt baseline.
#[test]
fn combined_call_all_24_decisions_and_19_product_mutations_match() {
    let corpus: Value = serde_json::from_str(COMPOSE_CORPUS).expect("compose corpus");
    let fixtures = corpus["fixtures"].as_array().expect("fixtures");
    assert_eq!(fixtures.len(), 24, "full #139 corpus changed");
    for fixture in fixtures {
        let id = fixture["id"].as_str().expect("id");
        let outcome = compose(fixture);
        assert_eq!(
            outcome.decision().as_str(),
            fixture["expected"]["decision"].as_str().expect("decision"),
            "{id}: decision"
        );
        if outcome.decision() != CompositionDecision::FallbackBaseline {
            assert_eq!(
                outcome.rendered(),
                fixture["expected"]["rendered"].as_str().expect("rendered"),
                "{id}: rendered"
            );
        }
        let expected_codes: Vec<&str> = fixture["expected"]["error_codes"]
            .as_array()
            .expect("error codes")
            .iter()
            .map(|code| code.as_str().expect("error code"))
            .collect();
        assert_eq!(
            outcome.error_code_strs(),
            expected_codes,
            "{id}: error codes"
        );
        assert_eq!(
            outcome.delivery(),
            DeliveryFlags::dpr_default(),
            "{id}: flags"
        );
    }

    rejected_mutation(&corpus, "CC-14", "protected_missing", |fixture| {
        fixture["candidate"]["derivation"][0]["output_text"] = "Call anuraj.".into();
    });
    rejected_mutation(&corpus, "CC-01", "invented_content_accept", |fixture| {
        fixture["candidate"]["derivation"] = serde_json::json!([{
            "kind": "keep", "source_provider": "provider_a",
            "source_text": "ship it exclamation point",
            "output_text": "Deploy to production now!",
            "conversion_id": null, "label": null
        }]);
    });
    rejected_mutation(&corpus, "CC-01", "stale_fingerprint", |fixture| {
        fixture["candidate"]["base_fingerprint"] = format!("sha256:{}", "a".repeat(64)).into();
    });
    rejected_mutation(&corpus, "CC-01", "unknown_conversion", |fixture| {
        fixture["candidate"]["conversions"][0]["id"] = "hey→Restart".into();
    });
    rejected_mutation(&corpus, "CC-20", "non_closed_header", |fixture| {
        fixture["candidate"]["derivation"][0]["output_text"] = "Edge Cases:\n".into();
    });
    rejected_mutation(&corpus, "CC-01", "empty_derivation", |fixture| {
        fixture["candidate"]["derivation"] = serde_json::json!([]);
    });
    rejected_mutation(&corpus, "CC-18", "natural_structural_label", |fixture| {
        fixture["policy"] = "natural".into();
        fixture["candidate"]["derivation"][0]["output_text"] = "Goal:\nPlease send notes.".into();
    });
    rejected_mutation(&corpus, "CC-01", "unverifiable_span", |fixture| {
        fixture["candidate"]["derivation"][0]["source_text"] = "not present in source xyz".into();
    });
    rejected_mutation(&corpus, "CC-14", "protected_name", |fixture| {
        fixture["cloud_outcome"] = "succeeded".into();
        fixture["candidate"]["derivation"][0]["output_text"] =
            "Call anuraj about the release.".into();
    });
    rejected_mutation(&corpus, "CC-01", "remove_without_removals", |fixture| {
        fixture["candidate"]["removals"] = serde_json::json!([]);
        fixture["candidate"]["derivation"] = serde_json::json!([
            {"kind":"remove","source_provider":"provider_a","source_text":"ship","output_text":"","conversion_id":null,"label":null},
            {"kind":"keep","source_provider":"provider_a","source_text":"it exclamation point","output_text":"It!","conversion_id":null,"label":null}
        ]);
    });
    rejected_mutation(&corpus, "CC-01", "convert_cue_missing", |fixture| {
        fixture["candidate"]["conversions"][0]["source_span_text"] = "ship it".into();
        fixture["candidate"]["derivation"][1]["source_text"] = "ship it".into();
    });
    rejected_mutation(&corpus, "CC-01", "double_keep_overlap", |fixture| {
        fixture["candidate"]["conversions"] = serde_json::json!([]);
        fixture["candidate"]["derivation"] = serde_json::json!([
            {"kind":"keep","source_provider":"provider_a","source_text":"ship it","output_text":"Ship it","conversion_id":null,"label":null},
            {"kind":"keep","source_provider":"provider_a","source_text":"ship it","output_text":" ship it","conversion_id":null,"label":null}
        ]);
    });
    for (name, breaks) in [
        ("natural_multiparagraph", serde_json::json!(["\n\n"])),
        ("natural_adjacent_newlines", serde_json::json!(["\n", "\n"])),
    ] {
        rejected_mutation(&corpus, "CC-18", name, |fixture| {
            fixture["candidate"]["layout"] =
                serde_json::json!({"decision":"natural","certainty":"clear"});
            let mut derivation = vec![serde_json::json!({
                "kind":"keep","source_provider":"provider_a",
                "source_text":"hey can you send the notes",
                "output_text":"Hey, can you send the notes",
                "conversion_id":null,"label":null
            })];
            for output in breaks.as_array().expect("breaks") {
                derivation.push(serde_json::json!({"kind":"layout_break","source_provider":null,"source_text":"","output_text":output,"conversion_id":null,"label":null}));
            }
            derivation.push(serde_json::json!({
                "kind":"keep","source_provider":"provider_a",
                "source_text":"when you get a chance",
                "output_text":"when you get a chance?",
                "conversion_id":null,"label":null
            }));
            fixture["candidate"]["derivation"] = Value::Array(derivation);
        });
    }
    rejected_mutation(&corpus, "CC-18", "natural_keep_edge_newlines", |fixture| {
        fixture["candidate"]["layout"] =
            serde_json::json!({"decision":"natural","certainty":"clear"});
        fixture["candidate"]["derivation"] = serde_json::json!([
            {"kind":"keep","source_provider":"provider_a","source_text":"hey can you send the notes","output_text":"Hey, can you send the notes\n","conversion_id":null,"label":null},
            {"kind":"keep","source_provider":"provider_a","source_text":"when you get a chance","output_text":"\nwhen you get a chance?","conversion_id":null,"label":null}
        ]);
    });
    rejected_mutation(&corpus, "CC-01", "reconciliation_mismatch", |fixture| {
        fixture["candidate"]["reconciliation"]["selected_provider"] = "provider_b".into();
    });
    rejected_mutation(&corpus, "CC-18", "keep_rephrase_drops_words", |fixture| {
        fixture["candidate"]["derivation"] = serde_json::json!([{
            "kind":"keep","source_provider":"provider_a",
            "source_text":"hey can you send the notes when you get a chance",
            "output_text":"Send the notes.","conversion_id":null,"label":null
        }]);
    });
    rejected_mutation(&corpus, "CC-18", "omit_undeclared_words", |fixture| {
        fixture["candidate"]["removals"] = serde_json::json!([]);
        fixture["candidate"]["derivation"] = serde_json::json!([{
            "kind":"keep","source_provider":"provider_a","source_text":"send the notes",
            "output_text":"Send the notes.","conversion_id":null,"label":null
        }]);
    });
    rejected_mutation(&corpus, "CC-18", "reverse_source_order", |fixture| {
        fixture["candidate"]["removals"] = serde_json::json!([]);
        fixture["candidate"]["derivation"] = serde_json::json!([
            {"kind":"keep","source_provider":"provider_a","source_text":"when you get a chance","output_text":"When you get a chance ","conversion_id":null,"label":null},
            {"kind":"keep","source_provider":"provider_a","source_text":"hey can you send the notes","output_text":"Hey can you send the notes","conversion_id":null,"label":null}
        ]);
    });
}

struct RecordingDelivery {
    calls: Arc<AtomicUsize>,
    delivered: Arc<Mutex<Vec<String>>>,
    clock: Option<(ControlledClock, Arc<AtomicU64>)>,
}

impl DeliveryAdapter for RecordingDelivery {
    fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.delivered
            .lock()
            .expect("delivery lock")
            .push(transcript.0);
        if let Some((clock, at)) = &self.clock {
            at.store(clock.millis.load(Ordering::SeqCst), Ordering::SeqCst);
        }
        Box::pin(async { Ok(DeliveryOutcome::compositor_submitted()) })
    }
}

#[derive(Clone)]
struct ControlledClock {
    millis: Arc<AtomicU64>,
}

impl ControlledClock {
    fn at(millis: u64) -> Self {
        Self {
            millis: Arc::new(AtomicU64::new(millis)),
        }
    }
}

impl DprPipelineClock for ControlledClock {
    fn elapsed(&self) -> Duration {
        Duration::from_millis(self.millis.load(Ordering::SeqCst))
    }
}

struct CannedCloud {
    calls: Arc<AtomicUsize>,
    clock: ControlledClock,
    complete_at: u64,
    result: Mutex<Option<DprCloudAttempt>>,
}

impl DprCloudBoundary for CannedCloud {
    fn attempt<'a>(
        &'a self,
        _credential: &'a Credential,
        _request: DprCloudRequest<'a>,
        remaining: Duration,
    ) -> DprCloudFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let deadline = self
            .clock
            .millis
            .load(Ordering::SeqCst)
            .saturating_add(u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX));
        self.clock
            .millis
            .store(self.complete_at.min(deadline), Ordering::SeqCst);
        let result = if self.complete_at > deadline {
            DprCloudAttempt::failure(DprCloudErrorClass::DeadlineExceeded)
        } else {
            self.result
                .lock()
                .expect("cloud result")
                .take()
                .expect("one attempt")
        };
        Box::pin(async move { result })
    }
}

fn delivery(
    clock: Option<ControlledClock>,
) -> (RecordingDelivery, Arc<AtomicUsize>, Arc<AtomicU64>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let at = Arc::new(AtomicU64::new(u64::MAX));
    (
        RecordingDelivery {
            calls: Arc::clone(&calls),
            delivered: Arc::new(Mutex::new(Vec::new())),
            clock: clock.map(|clock| (clock, Arc::clone(&at))),
        },
        calls,
        at,
    )
}

fn accepted_candidate(source: &str, output: &str) -> StructuredCandidate {
    parse_structured_candidate_json(
        serde_json::json!({
            "schema_version":"1",
            "base_fingerprint":voisu_core::text_sha256_fingerprint(source),
            "reconciliation":{"selected_provider":"provider_a","reason":"configured_primary_rank"},
            "removals":[],"conversions":[],
            "layout":{"decision":"natural","certainty":"clear"},"labels":[],
            "derivation":[{"kind":"keep","source_provider":"provider_a","source_text":source,"output_text":output,"conversion_id":null,"label":null}]
        })
        .to_string()
        .as_bytes(),
    )
    .expect("accepted candidate")
}

/// Ship gates 2–5: injected time enforces the 1500 ms bound; one candidate
/// still passes the sole compose gate; one stale candidate and one slow result
/// fall back after exactly one attempt; Delivery and production diagnostics
/// never expose a late-upgrade path.
#[tokio::test]
async fn orchestration_deadline_compose_call_count_delivery_and_diagnostics() {
    let source = "hello from voisu";
    let sources = [
        ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: source.to_owned(),
            primary: true,
        },
        ComposeSource {
            provider: SttProvider::ProviderB,
            available: true,
            text: "hello from voice you".to_owned(),
            primary: false,
        },
    ];
    let selection = SourceSelection {
        selected_provider: SttProvider::ProviderA,
        reason: "configured_primary_rank".to_owned(),
    };
    let credential = Credential::new("hermetic-secret".to_owned()).expect("credential");

    for (name, complete_at, candidate, expected, decision) in [
        (
            "accepted",
            200,
            accepted_candidate(source, "Hello from voisu!"),
            "Hello from voisu!",
            CompositionDecision::Accept,
        ),
        (
            "late",
            5_000,
            accepted_candidate(source, "Hello from voisu!"),
            "Hello from voisu.",
            CompositionDecision::FallbackBaseline,
        ),
        (
            "stale",
            200,
            {
                let mut stale = accepted_candidate(source, "Hello from voisu!");
                stale.base_fingerprint = format!("sha256:{}", "0".repeat(64));
                stale
            },
            "Hello from voisu.",
            CompositionDecision::FallbackBaseline,
        ),
    ] {
        let clock = ControlledClock::at(0);
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let cloud = CannedCloud {
            calls: Arc::clone(&cloud_calls),
            clock: clock.clone(),
            complete_at,
            result: Mutex::new(Some(DprCloudAttempt::success(candidate))),
        };
        let (mut delivery, delivery_calls, delivery_at) = delivery(Some(clock.clone()));
        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Adaptive,
                english_eligible: true,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Ready {
                    boundary: &cloud,
                    credential: &credential,
                },
                clock: &clock,
                small_edit_contract: false,
            },
            &mut delivery,
        )
        .await;
        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1, "{name}: cloud calls");
        assert_eq!(
            delivery_calls.load(Ordering::SeqCst),
            1,
            "{name}: Delivery calls"
        );
        assert!(
            delivery_at.load(Ordering::SeqCst) <= 1_500,
            "{name}: late Delivery"
        );
        assert_eq!(completion.rendered, expected, "{name}: rendered");
        assert_eq!(completion.compose_decision, decision, "{name}: decision");
        assert_eq!(
            completion.delivery_flags,
            DeliveryFlags::dpr_default(),
            "{name}: flags"
        );
        assert_eq!(
            completion.diagnostic.mode(),
            DprDiagnosticMode::Production,
            "{name}: mode"
        );
        let diagnostic = serde_json::to_string(&completion.diagnostic).expect("diagnostic JSON");
        assert!(
            !diagnostic.contains("late_evaluation"),
            "{name}: eval lane in production"
        );
        assert!(
            !diagnostic.contains("candidate_text"),
            "{name}: retained text in production"
        );
        assert!(
            !diagnostic.contains("apply_late"),
            "{name}: apply-late path in production"
        );
    }
}

/// Ship gate 3 call-budget matrix: every Natural/not-allowed case remains at
/// zero calls, and representative provider/transport failures consume the one
/// allowed attempt without retrying or falling through to another provider.
#[tokio::test]
async fn cloud_call_budget_covers_zero_call_and_failed_attempt_paths() {
    let source = "hello from voisu";
    let credential = Credential::new("hermetic-secret".to_owned()).expect("credential");
    for (name, selected_source, alternate_source, policy, surface_hint, expected_route) in [
        (
            "natural-simple",
            source,
            None,
            RenderingPolicy::Natural,
            None,
            RenderingRoute::DeterministicLocal,
        ),
        (
            "natural-dispute",
            source,
            Some("hello from voice you"),
            RenderingPolicy::Natural,
            None,
            RenderingRoute::DeterministicLocal,
        ),
        (
            "adaptive-default-local",
            source,
            None,
            RenderingPolicy::Adaptive,
            None,
            RenderingRoute::DeterministicLocal,
        ),
        (
            "structured-default-local",
            source,
            None,
            RenderingPolicy::Structured,
            None,
            RenderingRoute::DeterministicLocal,
        ),
        (
            "adaptive-preformatted-literal",
            "1. first\n2. second",
            None,
            RenderingPolicy::Adaptive,
            None,
            RenderingRoute::LiteralIdentity,
        ),
        (
            "structured-preformatted-literal",
            "1. first\n2. second",
            None,
            RenderingPolicy::Structured,
            None,
            RenderingRoute::LiteralIdentity,
        ),
        (
            "adaptive-command-literal",
            "cargo test",
            None,
            RenderingPolicy::Adaptive,
            Some(SurfaceHint::Terminal),
            RenderingRoute::LiteralIdentity,
        ),
        (
            "structured-command-literal",
            "cargo test",
            None,
            RenderingPolicy::Structured,
            Some(SurfaceHint::Terminal),
            RenderingRoute::LiteralIdentity,
        ),
    ] {
        let mut sources = vec![ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: selected_source.to_owned(),
            primary: true,
        }];
        if let Some(alternate) = alternate_source {
            sources.push(ComposeSource {
                provider: SttProvider::ProviderB,
                available: true,
                text: alternate.to_owned(),
                primary: false,
            });
        }
        let provider_state = if alternate_source.is_some() {
            ProviderState::SemanticDisagreement
        } else {
            ProviderState::SingleProvider
        };
        let clock = ControlledClock::at(0);
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let cloud = CannedCloud {
            calls: Arc::clone(&cloud_calls),
            clock: clock.clone(),
            complete_at: 10,
            result: Mutex::new(Some(DprCloudAttempt::failure(DprCloudErrorClass::Http5xx))),
        };
        let (mut delivery, delivery_calls, _) = delivery(Some(clock.clone()));
        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source,
                sources: &sources,
                source_selection: &SourceSelection {
                    selected_provider: SttProvider::ProviderA,
                    reason: "configured_primary_rank".to_owned(),
                },
                provider_state,
                policy,
                english_eligible: true,
                surface_hint,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Ready {
                    boundary: &cloud,
                    credential: &credential,
                },
                clock: &clock,
                small_edit_contract: false,
            },
            &mut delivery,
        )
        .await;
        assert_eq!(cloud_calls.load(Ordering::SeqCst), 0, "{name}: cloud");
        assert!(!completion.cloud_attempted, "{name}: attempted");
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1, "{name}: Delivery");
        assert_eq!(completion.routing.route, expected_route, "{name}: route");
        assert_eq!(
            completion.routing.cloud_request,
            CloudRequest::NotAllowed,
            "{name}: cloud permission"
        );
        assert_eq!(
            completion.delivery_flags,
            DeliveryFlags::dpr_default(),
            "{name}: flags"
        );
    }

    let sources = [
        ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: source.to_owned(),
            primary: true,
        },
        ComposeSource {
            provider: SttProvider::ProviderB,
            available: true,
            text: "hello from voice you".to_owned(),
            primary: false,
        },
    ];
    for error in [
        DprCloudErrorClass::Http5xx,
        DprCloudErrorClass::RateLimited,
        DprCloudErrorClass::Transport,
    ] {
        let clock = ControlledClock::at(0);
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let cloud = CannedCloud {
            calls: Arc::clone(&cloud_calls),
            clock: clock.clone(),
            complete_at: 100,
            result: Mutex::new(Some(DprCloudAttempt::failure(error))),
        };
        let (mut delivery, delivery_calls, _) = delivery(Some(clock.clone()));
        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &SourceSelection {
                    selected_provider: SttProvider::ProviderA,
                    reason: "configured_primary_rank".to_owned(),
                },
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Adaptive,
                english_eligible: true,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Ready {
                    boundary: &cloud,
                    credential: &credential,
                },
                clock: &clock,
                small_edit_contract: false,
            },
            &mut delivery,
        )
        .await;
        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1, "{error:?}: attempts");
        assert_eq!(
            delivery_calls.load(Ordering::SeqCst),
            1,
            "{error:?}: Delivery"
        );
        assert_eq!(
            completion.delivery_flags,
            DeliveryFlags::dpr_default(),
            "{error:?}: flags"
        );
        assert_eq!(completion.rendered, "Hello from voisu.");
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert_eq!(completion.cloud_error, Some(error));
    }
}

fn format_edit_candidate(
    source: &str,
    start: usize,
    end: usize,
    before: &str,
    after: &str,
    kind: &str,
) -> FormatEditCandidate {
    parse_format_edit_candidate_json(
        serde_json::json!({
            "version": "1",
            "base_fingerprint": voisu_core::text_sha256_fingerprint(source),
            "edits": [{
                "start_utf8": start,
                "end_utf8": end,
                "before": before,
                "after": after,
                "kind": kind,
            }],
        })
        .to_string()
        .as_bytes(),
    )
    .expect("format edits")
}

/// Formatting apply may introduce wording that is not in the Source Transcript,
/// but protected facts, unsupported headings, artifacts, and empty/summary
/// still fall back to the local baseline.
#[tokio::test]
async fn formatting_apply_relaxes_lexical_source_words_without_dropping_safety() {
    let credential = Credential::new("hermetic-secret".to_owned()).expect("credential");
    let wording_source = "goal pls ship the rust parser";
    let wording_sources = [ComposeSource {
        provider: SttProvider::ProviderA,
        available: true,
        text: wording_source.to_owned(),
        primary: true,
    }];
    let wording_selection = SourceSelection {
        selected_provider: SttProvider::ProviderA,
        reason: "only_available".to_owned(),
    };
    let clock = ControlledClock::at(0);
    let cloud_calls = Arc::new(AtomicUsize::new(0));
    let cloud = CannedCloud {
        calls: Arc::clone(&cloud_calls),
        clock: clock.clone(),
        complete_at: 200,
        result: Mutex::new(Some(DprCloudAttempt::format_edits(format_edit_candidate(
            wording_source,
            5,
            8,
            "pls",
            "Please",
            "bounded_wording",
        )))),
    };
    let (mut wording_delivery, delivery_calls, _) = delivery(Some(clock.clone()));
    let accepted = dpr_transform_and_deliver(
        DprTransformInput {
            selected_source: wording_source,
            sources: &wording_sources,
            source_selection: &wording_selection,
            provider_state: ProviderState::SemanticDisagreement,
            policy: RenderingPolicy::Adaptive,
            english_eligible: true,
            surface_hint: None,
            process_hint: None,
            timing: None,
            protected_tokens: &[],
            cloud: DprCloudCapability::Ready {
                boundary: &cloud,
                credential: &credential,
            },
            clock: &clock,
            small_edit_contract: true,
        },
        &mut wording_delivery,
    )
    .await;
    assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
    assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
    assert_eq!(accepted.rendered, "goal Please ship the rust parser");
    assert_eq!(accepted.compose_decision, CompositionDecision::Accept);

    let unsafe_source = "goal do not deploy https://example.test/a";
    let unsafe_sources = [ComposeSource {
        provider: SttProvider::ProviderA,
        available: true,
        text: unsafe_source.to_owned(),
        primary: true,
    }];
    let unsafe_selection = SourceSelection {
        selected_provider: SttProvider::ProviderA,
        reason: "only_available".to_owned(),
    };
    let url_start = unsafe_source.find("https://example.test/a").unwrap();
    let clock = ControlledClock::at(0);
    let cloud_calls = Arc::new(AtomicUsize::new(0));
    let cloud = CannedCloud {
        calls: Arc::clone(&cloud_calls),
        clock: clock.clone(),
        complete_at: 200,
        result: Mutex::new(Some(DprCloudAttempt::format_edits(format_edit_candidate(
            unsafe_source,
            url_start,
            url_start + "https://example.test/a".len(),
            "https://example.test/a",
            "https://evil.test/a",
            "bounded_wording",
        )))),
    };
    let (mut unsafe_delivery, delivery_calls, _) = delivery(Some(clock.clone()));
    let baseline = organize_local_baseline(
        unsafe_source,
        &LocalBaselineOptions {
            policy: RenderingPolicy::Adaptive,
            route: RenderingRoute::LocalWithOptionalCloud,
            timing: None,
        },
    );
    let rejected = dpr_transform_and_deliver(
        DprTransformInput {
            selected_source: unsafe_source,
            sources: &unsafe_sources,
            source_selection: &unsafe_selection,
            provider_state: ProviderState::SemanticDisagreement,
            policy: RenderingPolicy::Adaptive,
            english_eligible: true,
            surface_hint: None,
            process_hint: None,
            timing: None,
            protected_tokens: &["not", "https://example.test/a"],
            cloud: DprCloudCapability::Ready {
                boundary: &cloud,
                credential: &credential,
            },
            clock: &clock,
            small_edit_contract: true,
        },
        &mut unsafe_delivery,
    )
    .await;
    assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
    assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rejected.rendered, baseline.rendered());
    assert_eq!(
        rejected.compose_decision,
        CompositionDecision::FallbackBaseline
    );
}

/// Ship gate 6 persistence half: all policies round-trip through the real CLI.
/// The Recording-consumption half is asserted at the production daemon seam
/// named in this module's run documentation.
#[test]
fn all_three_policies_persist_via_cli() {
    let temp = tempfile::tempdir().expect("config tempdir");
    let binary = env!("CARGO_BIN_EXE_voisu");
    let run = |arguments: &[&str]| {
        Command::new(binary)
            .args(arguments)
            .env("XDG_CONFIG_HOME", temp.path())
            .env_remove("VOISU_ENABLE_DPR")
            .env_remove("VOISU_ENABLE_QWEN_FORMAT")
            .output()
            .expect("run voisu CLI")
    };
    for policy in ["natural", "adaptive", "structured"] {
        let set = run(&["rendering", policy]);
        assert!(
            set.status.success(),
            "set {policy}: {}",
            String::from_utf8_lossy(&set.stderr)
        );
        let query = run(&["rendering"]);
        assert!(query.status.success(), "query {policy}");
        assert_eq!(
            String::from_utf8_lossy(&query.stdout).trim(),
            format!("rendering policy: {policy}")
        );
    }
}

/// Ship gate 8's in-process behavior half: the existing Smart Writing gate
/// still formats and performs one ordinary Delivery. The production flag-off
/// dispatch and null DPR diagnostic are asserted in the daemon test named in
/// this module's run documentation.
#[tokio::test]
async fn dpr_flag_off_smart_writing_regression() {
    let (mut delivery, calls, _) = delivery(None);
    let languages = ResolvedRecordingLanguages::new(vec![(Provider::Groq, "en".to_owned())]);
    let completion = final_transform_and_deliver(
        FinalTransformInput {
            validated_transcript: "hello world",
            writing_mode: WritingMode::Smart,
            languages: &languages,
            grammar: GrammarGateCapability::Unavailable,
            dictionary_terms: &[],
            protected_names: &[],
            credential: CredentialGateEvidence::default(),
        },
        &mut delivery,
    )
    .await;
    assert_eq!(completion.rendered, "Hello world.");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!completion.diagnostic.request_began);
    assert_eq!(
        completion.diagnostic.outcome,
        voisu_core::SmartWritingOutcome::FormattingOnly
    );
}
