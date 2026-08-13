//! Opt-in leftover-Goal live formatter test (#189).
//!
//! Default CI stays hermetic: the live path is `#[ignore]` and also requires
//! `VOISU_LIVE_GOAL_FORMAT=1`. Any other value or unset fails closed before
//! network. Packaging still defaults `VOISU_ENABLE_QWEN_FORMAT` off.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use voisu_app::dpr_cloud::{load_groq_credential, DprCloudClient, DprCloudErrorClass};
use voisu_app::dpr_pipeline::{
    dpr_protected_tokens, dpr_source_context, dpr_transform_and_deliver, DprCloudCapability,
    DprTransformInput, SystemDprPipelineClock,
};
use voisu_app::system::SecretToolStore;
use voisu_core::{
    leftover_admits_format_cloud, organize_local_baseline, route_intent, BoundaryFuture,
    CompositionDecision, DeliveryAdapter, DeliveryOutcome, IntentObservation, LocalBaselineOptions,
    Provider, ProviderState, RenderingPolicy, SourceTranscript, Transcript,
};

const LIVE_GOAL_FORMAT_ENV: &str = "VOISU_LIVE_GOAL_FORMAT";

/// Spoken leftover Goal / mixed notes. After local organize these still admit
/// a formatting cloud call. No dash-dash, first/second lists, or ordinary chat.
const LIVE_GOAL_FORMAT_FIXTURES: &[&str] = &[
    "Goal is to deploy the application right now",
    "goal ship the rust parser",
    "goal is to deploy the application right now context is the production cluster notes check the rollback",
];

const HALLUCINATED_OUTROS: &[&str] = &[
    "thank you for watching",
    "thanks for watching",
    "like and subscribe",
    "subtitles by",
    "transcribed by",
];

const PROMPT_ROLE_MARKERS: &[&str] = &["user:", "developer:"];

/// True only when `VOISU_LIVE_GOAL_FORMAT` is exactly `1`.
fn live_goal_format_enabled() -> bool {
    live_goal_format_value_enabled(std::env::var(LIVE_GOAL_FORMAT_ENV).ok().as_deref())
}

fn live_goal_format_value_enabled(value: Option<&str>) -> bool {
    value == Some("1")
}

fn leftover_baseline(spoken: &str) -> String {
    let routing = route_intent(&IntentObservation {
        policy: RenderingPolicy::Adaptive,
        primary_text: spoken.to_owned(),
        provider_state: ProviderState::SemanticDisagreement,
        surface_hint: None,
        process_hint: None,
        timing: None,
    });
    organize_local_baseline(
        spoken,
        &LocalBaselineOptions {
            policy: RenderingPolicy::Adaptive,
            route: routing.route,
            timing: None,
        },
    )
    .rendered()
    .to_owned()
}

fn has_hallucinated_outro(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    let tail = lower.trim_end_matches(|character: char| !character.is_alphanumeric());
    HALLUCINATED_OUTROS
        .iter()
        .any(|suffix| tail.ends_with(suffix) || tail.contains(suffix))
}

fn has_prompt_junk(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start().to_ascii_lowercase();
        PROMPT_ROLE_MARKERS
            .iter()
            .any(|marker| trimmed.starts_with(marker))
    })
}

fn content_words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| word.len() > 2)
        .map(str::to_ascii_lowercase)
        .filter(|word| !matches!(word.as_str(), "the" | "and" | "for"))
        .collect()
}

fn keeps_spoken_meaning(spoken: &str, delivered: &str) {
    let delivered_lower = delivered.to_ascii_lowercase();
    for word in content_words(spoken) {
        assert!(
            delivered_lower.contains(&word),
            "delivered text dropped spoken word {word:?}"
        );
    }
}

fn assert_closed_cloud_error(error: DprCloudErrorClass) {
    match error {
        DprCloudErrorClass::CredentialUnavailable
        | DprCloudErrorClass::DeadlineExceeded
        | DprCloudErrorClass::RequestInvalid
        | DprCloudErrorClass::HttpClient
        | DprCloudErrorClass::Http4xx
        | DprCloudErrorClass::RateLimited
        | DprCloudErrorClass::Http5xx
        | DprCloudErrorClass::Transport
        | DprCloudErrorClass::ResponseTooLarge
        | DprCloudErrorClass::ProviderEnvelope
        | DprCloudErrorClass::CandidateSchema => {}
    }
}

struct RecordingDelivery {
    calls: Arc<AtomicUsize>,
    delivered: Arc<Mutex<Vec<String>>>,
}

impl DeliveryAdapter for RecordingDelivery {
    fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.delivered
            .lock()
            .expect("delivery lock")
            .push(transcript.0);
        Box::pin(async { Ok(DeliveryOutcome::compositor_submitted()) })
    }
}

#[test]
fn live_goal_format_enabled_is_false_in_this_process() {
    assert!(
        !live_goal_format_enabled(),
        "default CI must not set VOISU_LIVE_GOAL_FORMAT=1"
    );
}

#[test]
fn live_goal_format_enabled_only_for_exact_1() {
    assert!(live_goal_format_value_enabled(Some("1")));
    for value in [
        None,
        Some(""),
        Some("0"),
        Some("true"),
        Some("TRUE"),
        Some("yes"),
        Some(" 1"),
        Some("1 "),
        Some("2"),
        Some("on"),
    ] {
        assert!(
            !live_goal_format_value_enabled(value),
            "must stay closed for {value:?}"
        );
    }
}

#[test]
fn live_goal_format_fixtures_still_admit_after_organize() {
    for spoken in LIVE_GOAL_FORMAT_FIXTURES {
        let organized = leftover_baseline(spoken);
        assert!(
            leftover_admits_format_cloud(&organized),
            "fixture must still admit after organize: {spoken:?} -> {organized:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires Groq credentials, a live Qwen formatter call, and VOISU_LIVE_GOAL_FORMAT=1"]
async fn live_goal_format_sends_leftover_notes_through_production_path() {
    assert!(
        live_goal_format_enabled(),
        "set {LIVE_GOAL_FORMAT_ENV}=1; any other value fails closed before network"
    );

    let client = DprCloudClient::groq();
    let credential = load_groq_credential(&mut SecretToolStore);
    if let Err(error) = &client {
        eprintln!("dpr_cloud_client={}", error.as_str());
    }

    for spoken in LIVE_GOAL_FORMAT_FIXTURES {
        let baseline = leftover_baseline(spoken);
        assert!(
            leftover_admits_format_cloud(&baseline),
            "leftover must admit before the live call: {spoken:?}"
        );

        let context = dpr_source_context(
            &[SourceTranscript {
                provider: Provider::Groq,
                text: spoken.to_string(),
            }],
            &[],
        )
        .expect("source context");
        let protected = dpr_protected_tokens(&context.selected_source, &[]);
        let protected_refs: Vec<&str> = protected.iter().map(String::as_str).collect();
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            delivered: Arc::clone(&delivered),
        };
        let clock = SystemDprPipelineClock::from_validation_completed(Instant::now());
        let cloud = match (client.as_ref(), credential.as_ref()) {
            (Ok(client), Ok(credential)) => DprCloudCapability::Ready {
                boundary: client,
                credential,
            },
            _ => DprCloudCapability::Unavailable,
        };

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: &context.selected_source,
                sources: &context.sources,
                source_selection: &context.source_selection,
                // Dispute is the leftover-Goal production case that already
                // allows cloud; leftover admission then decides the formatter.
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Adaptive,
                english_eligible: true,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &protected_refs,
                cloud,
                clock: &clock,
                small_edit_contract: true,
            },
            &mut delivery,
        )
        .await;

        let delivered = delivered.lock().expect("delivery").clone();
        assert_eq!(
            delivery_calls.load(Ordering::SeqCst),
            1,
            "exactly one Delivery"
        );
        assert_eq!(delivered.len(), 1, "exactly one delivered Transcript");
        let transcript = &delivered[0];
        assert_eq!(transcript, &completion.rendered);
        assert_eq!(
            transcript,
            transcript.trim(),
            "delivered Transcript must not have leading or trailing whitespace"
        );
        assert!(!has_hallucinated_outro(transcript), "hallucinated outro");
        assert!(!has_prompt_junk(transcript), "prompt junk at line start");
        assert!(
            completion.delivery.is_ok(),
            "Delivery adapter must succeed"
        );

        eprintln!(
            "live_goal_format cloud_attempted={} compose_decision={:?} cloud_error={}",
            completion.cloud_attempted,
            completion.compose_decision,
            completion
                .cloud_error
                .map(DprCloudErrorClass::as_str)
                .unwrap_or("none")
        );

        if let Some(error) = completion.cloud_error {
            eprintln!("cloud_error={}", error.as_str());
            assert_closed_cloud_error(error);
            assert_eq!(
                transcript, &baseline,
                "fallback must deliver the local baseline"
            );
            continue;
        }

        if completion.compose_decision == CompositionDecision::Accept {
            assert!(
                transcript.to_ascii_lowercase().contains("goal"),
                "accepted cloud text must keep the spoken Goal license"
            );
            keeps_spoken_meaning(spoken, transcript);
            for line in transcript.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with('#') {
                    assert!(
                        trimmed.to_ascii_lowercase().contains("goal"),
                        "do not invent a heading without the word goal"
                    );
                }
            }
        } else {
            assert_eq!(
                transcript, &baseline,
                "non-accept path must deliver the local baseline"
            );
        }
    }
}
