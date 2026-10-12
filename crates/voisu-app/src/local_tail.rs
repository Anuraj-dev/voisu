//! Deterministic Local tail: dictionary replacements, local formatting, Delivery.
//!
//! Dictionary replacements and `format_validated` are local-only. Empty or
//! silence is explicit no-text, as are ASR outro hallucinations (the shared
//! Source Transcript sanitizer) and whole-transcript Whisper silence phrases.
//! Replay never receives a Delivery permit.

use std::collections::BTreeSet;

use voisu_core::{Transcript, WritingMode, format_validated};

use crate::local_worker::{CloudCapabilitySentinel, WorkerOutcome, is_silence_pcm};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalDecision {
    Transcript(String),
    NoText { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TailError {
    DuplicateDecision,
    UnauthorizedDelivery,
    DuplicateDelivery,
    CloudCapabilityUsed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryPermit {
    recording_id: String,
}

/// Single-use Delivery authorization for the live Recording. Replay never
/// occupies `live`.
#[derive(Clone, Debug, Default)]
pub struct DeliveryCoordinator {
    live: Option<String>,
    authorized: BTreeSet<String>,
    spent: BTreeSet<String>,
    decided: BTreeSet<String>,
}

impl DeliveryCoordinator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin_live(&mut self, recording_id: impl Into<String>) {
        self.live = Some(recording_id.into());
    }

    pub fn abandon_live(&mut self, recording_id: &str) {
        if self.live.as_deref() == Some(recording_id) {
            self.live = None;
        }
    }

    pub fn authorize_live(&mut self, recording_id: &str) -> Result<DeliveryPermit, TailError> {
        if self.spent.contains(recording_id) || self.authorized.contains(recording_id) {
            return Err(TailError::DuplicateDelivery);
        }
        match self.live.take() {
            Some(id) if id == recording_id => {
                self.authorized.insert(id.clone());
                Ok(DeliveryPermit { recording_id: id })
            }
            other => {
                self.live = other;
                Err(TailError::UnauthorizedDelivery)
            }
        }
    }

    pub fn confirm_delivery(
        &mut self,
        permit: DeliveryPermit,
        _transcript: &Transcript,
    ) -> Result<(), TailError> {
        if self.spent.contains(&permit.recording_id) {
            return Err(TailError::DuplicateDelivery);
        }
        if !self.authorized.remove(&permit.recording_id) {
            return Err(TailError::UnauthorizedDelivery);
        }
        self.spent.insert(permit.recording_id);
        Ok(())
    }

    fn remember_decision(&mut self, recording_id: &str) -> Result<(), TailError> {
        if !self.decided.insert(recording_id.to_owned()) {
            return Err(TailError::DuplicateDecision);
        }
        Ok(())
    }
}

/// One terminal decision: no-text, or hallucination sanitizing, dictionary,
/// then local formatting.
pub fn decide_transcript(
    recording_id: &str,
    pcm: &[u8],
    outcome: WorkerOutcome,
    user_terms: &[String],
    writing_mode: WritingMode,
    coordinator: &mut DeliveryCoordinator,
    sentinel: &CloudCapabilitySentinel,
) -> Result<TerminalDecision, TailError> {
    if !sentinel.local_path_clean() {
        return Err(TailError::CloudCapabilityUsed);
    }
    coordinator.remember_decision(recording_id)?;
    if is_silence_pcm(pcm) {
        return Ok(TerminalDecision::NoText {
            reason: "silence".to_owned(),
        });
    }
    match outcome {
        WorkerOutcome::NoText { reason, .. } => Ok(TerminalDecision::NoText { reason }),
        WorkerOutcome::Transcript { text, .. } => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Ok(TerminalDecision::NoText {
                    reason: "empty".to_owned(),
                });
            }
            let sanitized = voisu_core::sanitize_source_transcript_text(trimmed);
            if sanitized.trim().is_empty() {
                return Ok(TerminalDecision::NoText {
                    reason: "hallucinated suffix".to_owned(),
                });
            }
            if is_whole_silence_phrase(&sanitized) {
                return Ok(TerminalDecision::NoText {
                    reason: "silence hallucination".to_owned(),
                });
            }
            let replaced = apply_local_dictionary(&sanitized, user_terms);
            let formatted = format_validated(&replaced, writing_mode);
            let rendered = formatted.rendered().trim();
            if rendered.is_empty() {
                Ok(TerminalDecision::NoText {
                    reason: "empty".to_owned(),
                })
            } else {
                Ok(TerminalDecision::Transcript(rendered.to_owned()))
            }
        }
    }
}

/// Replay infers and validates; it never authorizes Delivery.
pub fn decide_replay(
    recording_id: &str,
    pcm: &[u8],
    outcome: WorkerOutcome,
    user_terms: &[String],
    writing_mode: WritingMode,
    coordinator: &mut DeliveryCoordinator,
    sentinel: &CloudCapabilitySentinel,
) -> Result<TerminalDecision, TailError> {
    let decision = decide_transcript(
        recording_id,
        pcm,
        outcome,
        user_terms,
        writing_mode,
        coordinator,
        sentinel,
    )?;
    if coordinator.authorize_live(recording_id).is_ok() {
        return Err(TailError::UnauthorizedDelivery);
    }
    Ok(decision)
}

/// Phrases local Whisper emits for near-silent audio. Matched only against the
/// WHOLE transcript, so a real sign-off after speech ("... Thank you.") is kept.
const WHISPER_SILENCE_PHRASES: [&str; 8] = [
    "thank you",
    "thanks",
    "thank you very much",
    "thank you so much",
    "thanks for listening",
    "you",
    "bye",
    "bye bye",
];

/// Lowercase, treat punctuation as a word break, collapse whitespace, then
/// compare to the closed silence-phrase set.
fn is_whole_silence_phrase(text: &str) -> bool {
    let spaced: String = text
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .to_lowercase();
    let normalized = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    WHISPER_SILENCE_PHRASES.contains(&normalized.as_str())
}

/// User-vocabulary replacements via the Cloud span-aware matcher, ungated.
pub fn apply_local_dictionary(text: &str, user_terms: &[String]) -> String {
    voisu_core::apply_user_vocabulary(text, user_terms, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(text: &str) -> WorkerOutcome {
        WorkerOutcome::Transcript {
            text: text.to_owned(),
            observed_device: "cpu".to_owned(),
            metrics: crate::local_worker::InferenceMetrics::default(),
        }
    }

    #[test]
    fn silence_is_explicit_no_text() {
        let mut coordinator = DeliveryCoordinator::new();
        let decision = decide_transcript(
            "rec-1",
            &[0, 0, 0, 0],
            outcome("invented"),
            &[],
            WritingMode::Literal,
            &mut coordinator,
            &CloudCapabilitySentinel::new(),
        )
        .unwrap();
        assert_eq!(
            decision,
            TerminalDecision::NoText {
                reason: "silence".to_owned()
            }
        );
    }

    #[test]
    fn empty_worker_text_is_no_text() {
        let mut coordinator = DeliveryCoordinator::new();
        let pcm = [1, 0, 2, 0];
        let decision = decide_transcript(
            "rec-1",
            &pcm,
            outcome("   "),
            &[],
            WritingMode::Literal,
            &mut coordinator,
            &CloudCapabilitySentinel::new(),
        )
        .unwrap();
        assert_eq!(
            decision,
            TerminalDecision::NoText {
                reason: "empty".to_owned()
            }
        );
    }

    #[test]
    fn dictionary_and_local_format_are_single_decision() {
        let mut coordinator = DeliveryCoordinator::new();
        let pcm = [1, 0];
        let decision = decide_transcript(
            "rec-1",
            &pcm,
            outcome("run the daemon reload now"),
            &["daemon-reload".to_owned()],
            WritingMode::Literal,
            &mut coordinator,
            &CloudCapabilitySentinel::new(),
        )
        .unwrap();
        assert_eq!(
            decision,
            TerminalDecision::Transcript("run the daemon-reload now".to_owned())
        );
        assert_eq!(
            decide_transcript(
                "rec-1",
                &pcm,
                outcome("again"),
                &[],
                WritingMode::Literal,
                &mut coordinator,
                &CloudCapabilitySentinel::new(),
            ),
            Err(TailError::DuplicateDecision)
        );
    }

    fn decide(text: &str, user_terms: &[String]) -> TerminalDecision {
        decide_transcript(
            "rec-1",
            &[1, 0],
            outcome(text),
            user_terms,
            WritingMode::Literal,
            &mut DeliveryCoordinator::new(),
            &CloudCapabilitySentinel::new(),
        )
        .unwrap()
    }

    fn no_text(reason: &str) -> TerminalDecision {
        TerminalDecision::NoText {
            reason: reason.to_owned(),
        }
    }

    #[test]
    fn anchored_outro_is_stripped_and_real_speech_kept() {
        assert_eq!(
            decide("Please send the report today. Thanks for watching!", &[]),
            TerminalDecision::Transcript("Please send the report today.".to_owned())
        );
    }

    #[test]
    fn outro_only_transcript_is_no_text() {
        for text in [
            "Thanks for watching!",
            "Thank you for watching.",
            "Thank you. Thanks for watching.",
            "Please like and subscribe",
        ] {
            assert_eq!(decide(text, &[]), no_text("hallucinated suffix"), "{text}");
        }
    }

    #[test]
    fn whole_transcript_silence_phrase_is_no_text() {
        for text in [
            "Thank you.",
            "thank you",
            "  THANK YOU!  ",
            "Thanks.",
            "Thank you very much.",
            "Thank you so much!",
            "Thanks for listening.",
            "You",
            "you.",
            "Bye.",
            "Bye bye.",
            "Bye-bye!",
            "Thank   you ...",
        ] {
            assert_eq!(
                decide(text, &[]),
                no_text("silence hallucination"),
                "{text:?}"
            );
        }
    }

    #[test]
    fn trailing_sign_off_after_real_speech_is_kept() {
        assert_eq!(
            decide("Please send the report. Thank you.", &[]),
            TerminalDecision::Transcript("Please send the report. Thank you.".to_owned())
        );
        assert_eq!(
            decide("Thank you for the update", &[]),
            TerminalDecision::Transcript("Thank you for the update".to_owned())
        );
    }

    #[test]
    fn normal_speech_is_unchanged() {
        assert_eq!(
            decide("The build finished in four minutes.", &[]),
            TerminalDecision::Transcript("The build finished in four minutes.".to_owned())
        );
    }

    #[test]
    fn dictionary_still_applies_after_outro_strip() {
        assert_eq!(
            decide(
                "run the daemon reload now. Thanks for watching.",
                &["daemon-reload".to_owned()]
            ),
            TerminalDecision::Transcript("run the daemon-reload now.".to_owned())
        );
    }

    #[test]
    fn dictionary_matches_hyphenated_multiword_and_punctuated_terms() {
        assert_eq!(
            apply_local_dictionary("run the daemon reload job", &["daemon-reload".to_owned()]),
            "run the daemon-reload job"
        );
        assert_eq!(
            apply_local_dictionary("open Node js docs", &["Node.js".to_owned()]),
            "open Node.js docs"
        );
        assert_eq!(
            apply_local_dictionary("open Claude code please", &["Claude Code".to_owned()]),
            "open Claude Code please"
        );
    }

    #[test]
    fn live_delivery_is_single_use() {
        let mut coordinator = DeliveryCoordinator::new();
        coordinator.begin_live("rec-1");
        let permit = coordinator.authorize_live("rec-1").unwrap();
        coordinator
            .confirm_delivery(permit.clone(), &Transcript("hi".into()))
            .unwrap();
        assert_eq!(
            coordinator.confirm_delivery(permit, &Transcript("hi".into())),
            Err(TailError::DuplicateDelivery)
        );
        assert_eq!(
            coordinator.authorize_live("rec-1"),
            Err(TailError::DuplicateDelivery)
        );
    }

    #[test]
    fn second_authorize_before_confirm_is_rejected() {
        let mut coordinator = DeliveryCoordinator::new();
        coordinator.begin_live("rec-1");
        let _permit = coordinator.authorize_live("rec-1").unwrap();
        assert_eq!(
            coordinator.authorize_live("rec-1"),
            Err(TailError::DuplicateDelivery)
        );
    }

    #[test]
    fn replay_never_authorizes_delivery() {
        let mut coordinator = DeliveryCoordinator::new();
        let pcm = [1, 0];
        let decision = decide_replay(
            "rec-replay",
            &pcm,
            outcome("hello"),
            &[],
            WritingMode::Literal,
            &mut coordinator,
            &CloudCapabilitySentinel::new(),
        )
        .unwrap();
        assert_eq!(decision, TerminalDecision::Transcript("hello".to_owned()));
        assert_eq!(
            coordinator.authorize_live("rec-replay"),
            Err(TailError::UnauthorizedDelivery)
        );
    }

    #[test]
    fn cloud_sentinel_blocks_the_tail() {
        let mut sentinel = CloudCapabilitySentinel::new();
        sentinel.read_cloud_credential();
        let mut coordinator = DeliveryCoordinator::new();
        assert_eq!(
            decide_transcript(
                "rec-1",
                &[1, 0],
                outcome("hello"),
                &[],
                WritingMode::Literal,
                &mut coordinator,
                &sentinel,
            ),
            Err(TailError::CloudCapabilityUsed)
        );
    }
}
