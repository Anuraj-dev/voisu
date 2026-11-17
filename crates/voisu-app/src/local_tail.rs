//! Deterministic Local tail: dictionary replacements, local formatting, Delivery.
//!
//! Dictionary replacements and formatting are local-only: the Developer Prompt
//! Rendering local baseline when DPR owns formatting (as in Cloud), otherwise
//! `format_validated` in the Writing Mode. Empty or
//! silence is explicit no-text, as are ASR outro hallucinations (the shared
//! Source Transcript sanitizer) and whole-transcript Whisper silence phrases.
//! Replay never receives a Delivery permit.

use std::collections::BTreeSet;

use voisu_core::{
    IntentObservation, ProviderState, RenderingPolicy, Transcript, WritingMode, format_validated,
    is_explicit_english,
};

use crate::local_worker::{CloudCapabilitySentinel, WorkerOutcome, is_silence_pcm};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalDecision {
    Transcript(String),
    NoText { reason: String },
}

/// How the Local tail formats a dictionary-corrected Transcript, snapshotted
/// per Recording. It mirrors Cloud's `dpr_owns_formatting` gate: with DPR on
/// for an English Recording the DPR local baseline formats (spoken "new line",
/// "new paragraph", marks, quotes and lists) and Writing Mode is not used;
/// otherwise `format_validated` runs in the Writing Mode. Tone stays with the
/// caller, after formatting, as in Cloud.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalFormatting {
    writing_mode: WritingMode,
    /// The Rendering Policy when DPR owns formatting.
    dpr: Option<RenderingPolicy>,
}

impl LocalFormatting {
    /// `dpr` is the Rendering Policy when DPR is enabled for this daemon.
    #[must_use]
    pub const fn new(
        writing_mode: crate::config::WritingMode,
        dpr: Option<RenderingPolicy>,
    ) -> Self {
        let writing_mode = match writing_mode {
            crate::config::WritingMode::Smart => WritingMode::Smart,
            crate::config::WritingMode::Literal => WritingMode::Literal,
        };
        Self { writing_mode, dpr }
    }

    /// DPR only formats English, so a Local model of another language falls
    /// back to the Writing Mode (Cloud's EnglishEligibility gate).
    #[must_use]
    pub fn for_model_language(self, language: &str) -> Self {
        if is_explicit_english(language) {
            self
        } else {
            Self { dpr: None, ..self }
        }
    }

    fn render(self, text: &str) -> String {
        match self.dpr {
            // Network-free: the same local baseline Cloud delivers when no
            // cloud formatting candidate is accepted. One Local source.
            Some(policy) => {
                let (_, baseline) =
                    voisu_cloud::dpr_local::route_and_organize_local(&IntentObservation {
                        policy,
                        primary_text: text.to_owned(),
                        provider_state: ProviderState::SingleProvider,
                        surface_hint: None,
                        process_hint: None,
                        timing: None,
                    });
                baseline.rendered().to_owned()
            }
            None => format_validated(text, self.writing_mode)
                .rendered()
                .to_owned(),
        }
    }
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
/// then local formatting ([`LocalFormatting`]).
pub fn decide_transcript(
    recording_id: &str,
    pcm: &[u8],
    outcome: WorkerOutcome,
    user_terms: &[String],
    formatting: LocalFormatting,
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
                    reason: "no words heard".to_owned(),
                });
            }
            if voisu_core::is_whisper_silence_phrase(&sanitized) {
                return Ok(TerminalDecision::NoText {
                    reason: "no words heard".to_owned(),
                });
            }
            let replaced = apply_local_dictionary(&sanitized, user_terms);
            let formatted = formatting.render(&replaced);
            let rendered = formatted.trim();
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
    formatting: LocalFormatting,
    coordinator: &mut DeliveryCoordinator,
    sentinel: &CloudCapabilitySentinel,
) -> Result<TerminalDecision, TailError> {
    let decision = decide_transcript(
        recording_id,
        pcm,
        outcome,
        user_terms,
        formatting,
        coordinator,
        sentinel,
    )?;
    if coordinator.authorize_live(recording_id).is_ok() {
        return Err(TailError::UnauthorizedDelivery);
    }
    Ok(decision)
}

/// Dictionary correction for Local: the Cloud span-aware exact matcher
/// (ungated, no word confidences), then correction by sound for names the
/// model wrote as it heard them ("voice you" -> "Voisu"). Cloud keeps exact
/// matching of user terms only. Local also corrects toward the built-in
/// developer glossary, after the user's terms, because a Local model hears
/// those names no better than the user's own (Cloud gets them as provider
/// keyterms and prompt instead).
pub fn apply_local_dictionary(text: &str, user_terms: &[String]) -> String {
    let terms = crate::dictionary::local_correction_terms(user_terms);
    let exact = voisu_core::apply_user_vocabulary(text, &terms, &[]);
    voisu_core::apply_phonetic_dictionary(&exact, &terms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LITERAL: LocalFormatting = LocalFormatting {
        writing_mode: WritingMode::Literal,
        dpr: None,
    };

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
            LITERAL,
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
            LITERAL,
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
            LITERAL,
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
                LITERAL,
                &mut coordinator,
                &CloudCapabilitySentinel::new(),
            ),
            Err(TailError::DuplicateDecision)
        );
    }

    fn decide(text: &str, user_terms: &[String]) -> TerminalDecision {
        decide_formatted(text, user_terms, LITERAL)
    }

    fn decide_formatted(
        text: &str,
        user_terms: &[String],
        formatting: LocalFormatting,
    ) -> TerminalDecision {
        decide_transcript(
            "rec-1",
            &[1, 0],
            outcome(text),
            user_terms,
            formatting,
            &mut DeliveryCoordinator::new(),
            &CloudCapabilitySentinel::new(),
        )
        .unwrap()
    }

    fn transcript(text: &str) -> TerminalDecision {
        TerminalDecision::Transcript(text.to_owned())
    }

    /// The packaged default: DPR on, Literal Writing Mode, English model.
    fn dpr_literal() -> LocalFormatting {
        LocalFormatting::new(
            crate::config::WritingMode::Literal,
            Some(RenderingPolicy::Adaptive),
        )
        .for_model_language("en")
    }

    /// A real Local dictation (distil-large-v3.5 writes it unpunctuated).
    const SPOKEN_BREAKS: &str = "Okay so I am now talking in the first line new line \
        this is a second line new line this is a third line new paragraph so this \
        is a new paragraph that should be visible";

    #[test]
    fn spoken_new_line_and_new_paragraph_break_the_local_transcript() {
        // DPR treats the second "new paragraph" as a cue too; that is its
        // designed behavior, shared with Cloud.
        assert_eq!(
            decide_formatted(SPOKEN_BREAKS, &[], dpr_literal()),
            transcript(
                "Okay so I am now talking in the first line.\nThis is a second line.\n\
                 This is a third line.\n\nSo this is a.\n\nThat should be visible."
            )
        );
    }

    #[test]
    fn local_dpr_formatting_matches_cloud_dpr_rendering() {
        let validated = "first point is cache second point is latency";
        let cloud = voisu_cloud::dpr_local::render_dpr_local(
            validated,
            &[voisu_core::SourceTranscript {
                provider: voisu_core::Provider::Groq,
                text: validated.to_owned(),
            }],
            &[],
            RenderingPolicy::Adaptive,
        )
        .expect("one Source Transcript with text");
        assert_eq!(
            decide_formatted(validated, &[], dpr_literal()),
            transcript(&cloud)
        );
    }

    #[test]
    fn dpr_formats_after_the_dictionary() {
        assert_eq!(
            decide_formatted(
                "ask voice you new line done",
                &["Voisu".to_owned()],
                dpr_literal()
            ),
            transcript("Ask Voisu.\nDone")
        );
    }

    #[test]
    fn without_dpr_local_keeps_the_writing_mode() {
        let rolled_back = LocalFormatting::new(crate::config::WritingMode::Literal, None);
        assert_eq!(
            decide_formatted(SPOKEN_BREAKS, &[], rolled_back),
            transcript(SPOKEN_BREAKS)
        );
        let smart = LocalFormatting::new(crate::config::WritingMode::Smart, None);
        assert_eq!(
            decide_formatted("ship it", &[], smart),
            transcript("Ship it.")
        );
    }

    #[test]
    fn a_non_english_local_model_does_not_use_dpr() {
        let formatting = LocalFormatting::new(
            crate::config::WritingMode::Literal,
            Some(RenderingPolicy::Adaptive),
        )
        .for_model_language("hi");
        assert_eq!(
            decide_formatted(SPOKEN_BREAKS, &[], formatting),
            transcript(SPOKEN_BREAKS)
        );
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
            assert_eq!(decide(text, &[]), no_text("no words heard"), "{text}");
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
            "Thank you. Thank you.",
            "Thank you thank you thank you.",
        ] {
            assert_eq!(decide(text, &[]), no_text("no words heard"), "{text:?}");
        }
    }

    #[test]
    fn trailing_sign_off_after_real_speech_is_kept() {
        assert_eq!(
            decide("Please send the report. Thank you.", &[]),
            TerminalDecision::Transcript("Please send the report. Thank you.".to_owned())
        );
        assert_eq!(
            decide("Thank you. See you tomorrow.", &[]),
            TerminalDecision::Transcript("Thank you. See you tomorrow.".to_owned())
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
    fn built_in_terms_correct_local_text_with_an_empty_dictionary() {
        assert_eq!(
            decide("we can apply this for voysu local", &[]),
            transcript("we can apply this for Voisu local")
        );
        assert_eq!(
            apply_local_dictionary("check the change log in Voisoo", &[]),
            "check the changelog in Voisu"
        );
    }

    #[test]
    fn a_user_term_heard_exactly_is_not_rewritten_to_a_built_in_term() {
        assert_eq!(
            apply_local_dictionary("ask voysu", &["voysu".to_owned()]),
            "ask voysu"
        );
    }

    #[test]
    fn built_in_terms_leave_ordinary_words_alone() {
        // Sound-alikes and spellings of built-in terms (Rust, Whisper, kernel,
        // trait, Docker, Plasma, Fedora, cargo, async, await, struct, Groq,
        // compiler) used as ordinary English.
        for sentence in [
            "the rust on the car and the cash in the drawer",
            "she said it in a whisper and closed the closet",
            "a kernel of truth and a colonel in the army",
            "the trade deal and the tray on the table and the trait",
            "cloud storage and the clod of dirt and the claws",
            "the docker at the port and the doctor at the clinic",
            "blood plasma and a plaza downtown and fedora hats",
            "the deployment of troops and the gateway arch",
            "the gross profit and the groceries",
            "cargo pants and the cardigan and the car go",
            "a sink and await the results and a weight",
            "the struck match and the structure",
            "the compiler and the complier and the system is down",
        ] {
            assert_eq!(apply_local_dictionary(sentence, &[]), sentence);
        }
    }

    #[test]
    fn local_dictionary_corrects_by_sound_after_exact_matches() {
        let dictionary = ["Voisu".to_owned(), "Hyprland".to_owned()];
        assert_eq!(
            apply_local_dictionary("voice you runs on hyper land", &dictionary),
            "Voisu runs on Hyprland"
        );
    }

    #[test]
    fn local_spelled_entry_is_matched_exactly_never_by_sound() {
        // "voice you" sounds like Voisu, but the user also spelled it out as
        // an entry. The sound pass alone never re-targets a spelled entry, in
        // either Dictionary order; the exact pass then writes it as spelled.
        for dictionary in [
            ["Voisu".to_owned(), "voice-you".to_owned()],
            ["voice-you".to_owned(), "Voisu".to_owned()],
        ] {
            assert_eq!(
                voisu_core::apply_phonetic_dictionary("say voice you", &dictionary),
                "say voice you"
            );
            assert_eq!(
                apply_local_dictionary("say voice you", &dictionary),
                "say voice-you"
            );
        }
    }

    #[test]
    fn local_tail_delivers_the_sound_corrected_transcript() {
        let mut coordinator = DeliveryCoordinator::new();
        let pcm = [1, 0, 2, 0];
        let decision = decide_transcript(
            "rec-1",
            &pcm,
            outcome("open voice you settings"),
            &["Voisu".to_owned()],
            LITERAL,
            &mut coordinator,
            &CloudCapabilitySentinel::new(),
        )
        .unwrap();
        assert_eq!(
            decision,
            TerminalDecision::Transcript("open Voisu settings".to_owned())
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
            LITERAL,
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
                LITERAL,
                &mut coordinator,
                &sentinel,
            ),
            Err(TailError::CloudCapabilityUsed)
        );
    }
}
