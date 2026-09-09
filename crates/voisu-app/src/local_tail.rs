//! Deterministic Local tail: dictionary replacements, local formatting, Delivery.
//!
//! Dictionary replacements and `format_validated` are local-only. Empty or
//! silence is explicit no-text. Replay never receives a Delivery permit.

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

/// One terminal decision: no-text, or dictionary then local formatting.
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
            let replaced = apply_local_dictionary(trimmed, user_terms);
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
