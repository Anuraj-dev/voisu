//! R7 locked English bakeoff: corpus, scoring, host profiles, go/no-go.
//!
//! Thresholds are frozen before measurements. This module never invents a
//! passing bakeoff result and never selects a model winner.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use voisu_core::{Transcript, stop_anchored_timings};

use super::bounds::{LOAD_DEADLINE, STOP_PROCESSING};
use super::protocol::Correlation;
use super::runtime::{RuntimeError, RuntimeFamily, refuse_production_weight_download};
use super::sandbox::CloudCapabilitySentinel;
use super::seams::{CaptureSeam, DeliverySeam, FakeCapture, FakeDelivery, SeamError};
use super::supervisor::{
    FakeWorker, SupervisorError, WorkerOutcome, WorkerState, WorkerSupervisor,
};

pub const CORPUS_CONTRACT_ID: &str = "voisu-local-asr-bakeoff-en-v1";
pub const CORPUS_VERSION: &str = "2026-09-08.r7.lock";

pub const MIN_SPEECH_RECORDINGS: usize = 100;
pub const MIN_NEGATIVE_RECORDINGS: usize = 20;
pub const MAX_TUNING_SPEECH: usize = 20;
pub const MIN_HELD_OUT_SPEECH: usize = 80;
pub const MIN_LONGEST_BAND: usize = 5;

pub const WER_OVERALL_MAX: f64 = 0.10;
pub const WER_STRATUM_MAX: f64 = 0.15;
pub const WARM_P95_UP_TO_30S_MS: u64 = 3_000;
pub const WARM_P95_UP_TO_120S_MS: u64 = 15_000;
pub const COLD_READY_MAX_MS: u64 = 60_000;
pub const SOAK_RECORDINGS: usize = 200;
pub const SOAK_WARMUP: usize = 20;
pub const SOAK_MIN_HOURS: u32 = 2;
pub const RSS_GROWTH_MAX_MIB: u64 = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostRole {
    FirstSupportedProduct,
    PilotNotProductProof,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostProfile {
    pub id: &'static str,
    pub role: HostRole,
    pub distro: &'static str,
    pub desktop: &'static str,
}

#[must_use]
pub fn locked_host_profiles() -> [HostProfile; 2] {
    [
        HostProfile {
            id: "fedora-kde-wayland",
            role: HostRole::FirstSupportedProduct,
            distro: "Fedora",
            desktop: "KDE Wayland",
        },
        HostProfile {
            id: "omarchy-arch-hyprland",
            role: HostRole::PilotNotProductProof,
            distro: "Arch/Omarchy",
            desktop: "Hyprland",
        },
    ]
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LockedThresholds {
    pub wer_overall_max: f64,
    pub wer_stratum_max: f64,
    pub warm_p95_up_to_30s_ms: u64,
    pub warm_p95_up_to_120s_ms: u64,
    pub stop_processing: Duration,
    pub cold_ready: Duration,
    pub soak_recordings: usize,
    pub soak_warmup: usize,
    pub soak_min_hours: u32,
    pub rss_growth_max_mib: u64,
}

#[must_use]
pub fn locked_thresholds() -> LockedThresholds {
    LockedThresholds {
        wer_overall_max: WER_OVERALL_MAX,
        wer_stratum_max: WER_STRATUM_MAX,
        warm_p95_up_to_30s_ms: WARM_P95_UP_TO_30S_MS,
        warm_p95_up_to_120s_ms: WARM_P95_UP_TO_120S_MS,
        stop_processing: STOP_PROCESSING,
        cold_ready: LOAD_DEADLINE,
        soak_recordings: SOAK_RECORDINGS,
        soak_warmup: SOAK_WARMUP,
        soak_min_hours: SOAK_MIN_HOURS,
        rss_growth_max_mib: RSS_GROWTH_MAX_MIB,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Split {
    Tuning,
    HeldOut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurationBand {
    OneToTen,
    TenToThirty,
    ThirtyToOneTwenty,
    OneTwentyToSixHundred,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaseKind {
    Speech,
    Negative,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CriticalKind {
    Number,
    Name,
    Negation,
    RequiredPhrase,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BakeoffCase {
    pub id: String,
    pub kind: CaseKind,
    pub split: Split,
    pub band: DurationBand,
    pub audio_hash: String,
    pub reference: String,
    pub pcm: Vec<u8>,
    pub speech: Duration,
    pub stratum: String,
    pub critical: Vec<(CriticalKind, String)>,
    pub scripted_hypothesis: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateVerdict {
    Go,
    NoGo,
    PendingEvidence,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WordError {
    pub deletions: usize,
    pub insertions: usize,
    pub substitutions: usize,
    pub reference_tokens: usize,
    pub error_rate: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StageTimings {
    pub capture_finalization_ms: u64,
    pub recovery_fsync_ms: u64,
    pub model_load_ms: u64,
    pub inference_ms: u64,
    pub formatting_ms: u64,
    pub delivery_ms: u64,
    pub recording_duration_ms: u64,
    pub stop_to_finalized_ms: u64,
    pub stop_to_delivered_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaseOutcome {
    pub id: String,
    pub kind: CaseKind,
    pub split: Split,
    pub delivered: bool,
    pub hypothesis: Option<String>,
    pub wer: Option<WordError>,
    pub critical_failures: Vec<String>,
    pub timings: Option<StageTimings>,
    pub observed_device: String,
    pub band: DurationBand,
    pub stratum: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BakeoffReport {
    pub corpus_contract: &'static str,
    pub corpus_version: &'static str,
    pub candidate: RuntimeFamily,
    pub winner_selected: bool,
    pub measurements_invented: bool,
    pub production_weights_downloaded: bool,
    pub corpus_lock_satisfied: bool,
    pub sample_counts: SampleCounts,
    pub p50_stop_to_delivered_ms: Option<u64>,
    pub p95_stop_to_delivered_ms: Option<u64>,
    pub max_stop_to_delivered_ms: Option<u64>,
    pub verdict: GateVerdict,
    pub cases: Vec<CaseOutcome>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoakEvidence {
    pub recordings: usize,
    pub hours: u32,
    pub warmup: usize,
    pub rss_growth_mib: u64,
    pub crash: bool,
    pub unreaped: bool,
    pub unexpected_network: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SampleCounts {
    pub speech: usize,
    pub negative: usize,
    pub tuning_speech: usize,
    pub held_out_speech: usize,
    pub longest_band: usize,
}

pub struct FeasibilityRunner<C, D> {
    capture: C,
    supervisor: WorkerSupervisor<FakeWorker>,
    delivery: D,
    sentinel: CloudCapabilitySentinel,
    candidate: RuntimeFamily,
}

impl FeasibilityRunner<FakeCapture, FakeDelivery> {
    #[must_use]
    pub fn harness(worker: FakeWorker) -> Self {
        let mut supervisor = WorkerSupervisor::absent();
        let _ = supervisor.attach_ready(worker, Instant::now());
        Self {
            capture: FakeCapture,
            supervisor,
            delivery: FakeDelivery::default(),
            sentinel: CloudCapabilitySentinel::new(),
            candidate: RuntimeFamily::WhisperCppProcess,
        }
    }
}

impl<C: CaptureSeam, D: DeliverySeam> FeasibilityRunner<C, D> {
    #[must_use]
    pub fn candidate(&self) -> RuntimeFamily {
        self.candidate
    }

    pub fn prepare(&mut self, correlation: Correlation) -> Result<Duration, SupervisorError> {
        if !self.sentinel.local_path_clean() {
            return Err(SupervisorError::Unavailable(
                "Cloud capability used on the Local path",
            ));
        }
        self.supervisor.prepare(correlation, Instant::now())
    }

    pub fn run_case(&mut self, case: &BakeoffCase) -> Result<CaseOutcome, RunnerError> {
        if refuse_production_weight_download() != Err(RuntimeError::ProductionDownloadForbidden) {
            return Err(RunnerError::CloudCapability);
        }
        if !self.sentinel.local_path_clean() {
            return Err(RunnerError::CloudCapability);
        }
        if self.supervisor.state() != WorkerState::Ready {
            return Err(RunnerError::Supervisor(SupervisorError::NotReady(
                self.supervisor.state(),
            )));
        }

        let capture_started = Instant::now();
        let finalized = self
            .capture
            .finalize(&case.id, &case.pcm, case.speech)
            .map_err(RunnerError::Seam)?;
        let capture_finalization_ms = millis(capture_started.elapsed());
        let recovery_fsync_ms = 0;
        if let Some(worker) = self.supervisor.child_mut() {
            worker.scripted_text = case.scripted_hypothesis.clone();
        }

        let inference_started = Instant::now();
        let correlation = Correlation {
            daemon_nonce: "bakeoff".into(),
            generation: 1,
            request_id: case.id.clone(),
            recording_id: case.id.clone(),
            model_receipt_hash: "harness-no-weights".into(),
        };
        let outcome = self
            .supervisor
            .transcribe(super::supervisor::TranscribeRequest {
                correlation,
                pcm: finalized.pcm,
            })
            .map_err(RunnerError::Supervisor)?;
        let inference_ms = millis(inference_started.elapsed());
        let formatting_ms = 0;
        let finalized_at = Instant::now();

        let (hypothesis, observed_device, no_text) = match outcome {
            WorkerOutcome::Transcript {
                text,
                observed_device,
            } => (Some(text), observed_device, false),
            WorkerOutcome::NoText {
                observed_device, ..
            } => (None, observed_device, true),
        };

        let mut delivered = false;
        let mut delivery_ms = 0;
        let mut delivered_at = finalized_at;
        if let Some(text) = hypothesis.as_ref() {
            let delivery_started = Instant::now();
            let token = self
                .delivery
                .authorize(&case.id)
                .map_err(RunnerError::Seam)?;
            self.delivery
                .deliver(token, &Transcript(text.clone()))
                .map_err(RunnerError::Seam)?;
            delivered = true;
            delivery_ms = millis(delivery_started.elapsed());
            delivered_at = Instant::now();
        }
        let stop = stop_anchored_timings(
            finalized.recording_start,
            finalized.utterance_end,
            finalized_at,
            delivered_at,
        );

        let wer = hypothesis
            .as_deref()
            .map(|text| align_words(&case.reference, text));
        let mut critical_failures = Vec::new();
        if case.kind == CaseKind::Negative {
            if delivered || hypothesis.is_some() {
                critical_failures.push("negative fixture inserted text".into());
            }
        } else if no_text {
            critical_failures.push("speech Recording produced no Transcript".into());
        }
        for (kind, token) in &case.critical {
            let haystack = hypothesis.as_deref().unwrap_or("");
            if !contains_contiguous_tokens(&tokenize(haystack), &tokenize(token)) {
                critical_failures.push(format!("{kind:?}:{token}"));
            }
        }

        Ok(CaseOutcome {
            id: case.id.clone(),
            kind: case.kind,
            split: case.split,
            delivered,
            hypothesis,
            wer,
            critical_failures,
            timings: Some(StageTimings {
                capture_finalization_ms,
                recovery_fsync_ms,
                model_load_ms: 0,
                inference_ms,
                formatting_ms,
                delivery_ms,
                recording_duration_ms: stop.recording_duration_ms,
                stop_to_finalized_ms: stop.stop_to_finalized_ms,
                stop_to_delivered_ms: delivered.then_some(stop.stop_to_delivered_ms),
            }),
            observed_device,
            band: case.band,
            stratum: case.stratum.clone(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerError {
    Supervisor(SupervisorError),
    Seam(SeamError),
    CloudCapability,
}

#[must_use]
pub fn sample_counts(cases: &[BakeoffCase]) -> SampleCounts {
    let mut counts = SampleCounts::default();
    for case in cases {
        match case.kind {
            CaseKind::Speech => {
                counts.speech += 1;
                match case.split {
                    Split::Tuning => counts.tuning_speech += 1,
                    Split::HeldOut => counts.held_out_speech += 1,
                }
                if case.band == DurationBand::OneTwentyToSixHundred {
                    counts.longest_band += 1;
                }
            }
            CaseKind::Negative => counts.negative += 1,
        }
    }
    counts
}

#[must_use]
pub fn corpus_lock_satisfied(cases: &[BakeoffCase]) -> bool {
    let counts = sample_counts(cases);
    if counts.speech < MIN_SPEECH_RECORDINGS
        || counts.negative < MIN_NEGATIVE_RECORDINGS
        || counts.tuning_speech > MAX_TUNING_SPEECH
        || counts.held_out_speech < MIN_HELD_OUT_SPEECH
        || counts.longest_band < MIN_LONGEST_BAND
    {
        return false;
    }
    let mut seen = BTreeSet::new();
    let mut tuning = BTreeSet::new();
    let mut held = BTreeSet::new();
    for case in cases {
        if !seen.insert(case.audio_hash.as_str()) {
            return false;
        }
        match case.split {
            Split::Tuning => {
                tuning.insert(case.audio_hash.as_str());
            }
            Split::HeldOut => {
                held.insert(case.audio_hash.as_str());
            }
        }
    }
    tuning.is_disjoint(&held)
}

#[must_use]
pub fn evaluate_report(
    candidate: RuntimeFamily,
    cases: &[BakeoffCase],
    outcomes: Vec<CaseOutcome>,
    notes: Vec<String>,
    soak: Option<&SoakEvidence>,
    native_packaged_runtime: bool,
) -> BakeoffReport {
    let counts = sample_counts(cases);
    let lock = corpus_lock_satisfied(cases);
    let mut delivered_ms: Vec<u64> = outcomes
        .iter()
        .filter(|row| row.split == Split::HeldOut && row.kind == CaseKind::Speech && row.delivered)
        .filter_map(|row| {
            row.timings
                .as_ref()
                .and_then(|timing| timing.stop_to_delivered_ms)
        })
        .collect();
    delivered_ms.sort_unstable();
    let mut notes = notes;
    // A false Go is forbidden: this harness never publishes a winner, and a
    // missing corpus is not a bakeoff result even if smoke WER is zero.
    let verdict = bakeoff_verdict(lock, &outcomes, soak, native_packaged_runtime, &mut notes);
    BakeoffReport {
        corpus_contract: CORPUS_CONTRACT_ID,
        corpus_version: CORPUS_VERSION,
        candidate,
        winner_selected: false,
        measurements_invented: false,
        production_weights_downloaded: false,
        corpus_lock_satisfied: lock,
        sample_counts: counts,
        p50_stop_to_delivered_ms: percentile_nearest_rank(&delivered_ms, 50),
        p95_stop_to_delivered_ms: percentile_nearest_rank(&delivered_ms, 95),
        max_stop_to_delivered_ms: delivered_ms.last().copied(),
        verdict,
        cases: outcomes,
        notes,
    }
}

fn bakeoff_verdict(
    lock: bool,
    outcomes: &[CaseOutcome],
    soak: Option<&SoakEvidence>,
    native_packaged_runtime: bool,
    notes: &mut Vec<String>,
) -> GateVerdict {
    if !lock {
        notes.push(
            "corpus lock unsatisfied; PendingEvidence is not a publishable bakeoff result".into(),
        );
        return GateVerdict::PendingEvidence;
    }
    if quality_gate_failed(outcomes) {
        notes.push("locked quality or Stop-to-Delivery gate failed".into());
        return GateVerdict::NoGo;
    }
    if let Some(soak) = soak {
        if soak_gate_failed(soak) {
            notes.push("locked soak gate failed".into());
            return GateVerdict::NoGo;
        }
    } else {
        notes.push("soak evidence missing; cannot publish a bakeoff result".into());
        return GateVerdict::PendingEvidence;
    }
    if !native_packaged_runtime {
        notes.push("packaged runtime evidence missing; cannot publish a bakeoff result".into());
        return GateVerdict::PendingEvidence;
    }
    notes.push("L2 harness never assigns Go; host-packaged proof is out of scope".into());
    GateVerdict::PendingEvidence
}

fn quality_gate_failed(outcomes: &[CaseOutcome]) -> bool {
    let held_speech: Vec<&CaseOutcome> = outcomes
        .iter()
        .filter(|row| row.split == Split::HeldOut && row.kind == CaseKind::Speech)
        .collect();
    if corpus_wer(&held_speech).is_some_and(|wer| wer > WER_OVERALL_MAX) {
        return true;
    }
    let mut strata = BTreeSet::new();
    for row in &held_speech {
        strata.insert(row.stratum.as_str());
    }
    for stratum in strata {
        let rows: Vec<&CaseOutcome> = held_speech
            .iter()
            .copied()
            .filter(|row| row.stratum == stratum)
            .collect();
        if corpus_wer(&rows).is_some_and(|wer| wer > WER_STRATUM_MAX) {
            return true;
        }
    }
    if held_speech
        .iter()
        .any(|row| !row.critical_failures.is_empty())
    {
        return true;
    }
    if outcomes
        .iter()
        .any(|row| row.kind == CaseKind::Negative && (row.delivered || row.hypothesis.is_some()))
    {
        return true;
    }
    if p95_for_bands(
        outcomes,
        &[DurationBand::OneToTen, DurationBand::TenToThirty],
    )
    .is_some_and(|p95| p95 > WARM_P95_UP_TO_30S_MS)
    {
        return true;
    }
    if p95_for_bands(outcomes, &[DurationBand::ThirtyToOneTwenty])
        .is_some_and(|p95| p95 > WARM_P95_UP_TO_120S_MS)
    {
        return true;
    }
    outcomes.iter().any(|row| {
        row.kind == CaseKind::Speech
            && row.delivered
            && row
                .timings
                .as_ref()
                .and_then(|timing| timing.stop_to_delivered_ms)
                .is_some_and(|ms| ms > millis(STOP_PROCESSING))
    })
}

fn corpus_wer(rows: &[&CaseOutcome]) -> Option<f64> {
    let mut errors = 0usize;
    let mut refs = 0usize;
    for row in rows {
        let Some(wer) = &row.wer else {
            continue;
        };
        errors += wer.insertions + wer.deletions + wer.substitutions;
        refs += wer.reference_tokens;
    }
    if refs == 0 {
        None
    } else {
        Some(errors as f64 / refs as f64)
    }
}

fn p95_for_bands(outcomes: &[CaseOutcome], bands: &[DurationBand]) -> Option<u64> {
    let mut samples: Vec<u64> = outcomes
        .iter()
        .filter(|row| {
            row.split == Split::HeldOut
                && row.kind == CaseKind::Speech
                && row.delivered
                && bands.contains(&row.band)
        })
        .filter_map(|row| {
            row.timings
                .as_ref()
                .and_then(|timing| timing.stop_to_delivered_ms)
        })
        .collect();
    samples.sort_unstable();
    percentile_nearest_rank(&samples, 95)
}

fn soak_gate_failed(soak: &SoakEvidence) -> bool {
    soak.recordings < SOAK_RECORDINGS
        || soak.hours < SOAK_MIN_HOURS
        || soak.warmup < SOAK_WARMUP
        || soak.rss_growth_mib > RSS_GROWTH_MAX_MIB
        || soak.crash
        || soak.unreaped
        || soak.unexpected_network
}

/// Locked scoring normalization: whitespace split, lowercase, strip a closed
/// punctuation set, then Levenshtein I/D/S over tokens. Punctuation and
/// semantic errors are reported separately by the host scorer; this function
/// is the WER contract.
#[must_use]
pub fn tokenize(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(normalize_token)
        .filter(|tok| !tok.is_empty())
        .collect()
}

/// Multiword critical entries must appear as a contiguous ordered sequence.
fn contains_contiguous_tokens(haystack: &[String], needle: &[String]) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn normalize_token(raw: &str) -> String {
    let mut tok = raw
        .trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '!' | '?'
            )
        })
        .to_owned();
    if tok.ends_with('.')
        && !tok.contains("://")
        && tok.matches('.').count() == 1
        && !tok.chars().any(|c: char| c.is_ascii_digit())
    {
        tok.pop();
    }
    tok.to_ascii_lowercase()
}

#[must_use]
pub fn align_words(reference: &str, hypothesis: &str) -> WordError {
    let reference_tokens = tokenize(reference);
    let hypothesis_tokens = tokenize(hypothesis);
    let (insertions, deletions, substitutions) =
        levenshtein_ops(&reference_tokens, &hypothesis_tokens);
    let n = reference_tokens.len();
    let errors = insertions + deletions + substitutions;
    let error_rate = if n == 0 {
        if hypothesis_tokens.is_empty() {
            0.0
        } else {
            1.0
        }
    } else {
        errors as f64 / n as f64
    };
    WordError {
        deletions,
        insertions,
        substitutions,
        reference_tokens: n,
        error_rate,
    }
}

fn levenshtein_ops(reference: &[String], hypothesis: &[String]) -> (usize, usize, usize) {
    let n = reference.len();
    let m = hypothesis.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for (i, row) in dp.iter_mut().enumerate().skip(1) {
        row[0] = i as u32;
    }
    for (j, cell) in dp[0].iter_mut().enumerate().skip(1) {
        *cell = j as u32;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = u32::from(reference[i - 1] != hypothesis[j - 1]);
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
        }
    }
    let mut i = n;
    let mut j = m;
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    let mut substitutions = 0usize;
    while i > 0 || j > 0 {
        if i > 0 && dp[i][j] == dp[i - 1][j] + 1 {
            deletions += 1;
            i -= 1;
        } else if j > 0 && dp[i][j] == dp[i][j - 1] + 1 {
            insertions += 1;
            j -= 1;
        } else {
            if i > 0 && j > 0 && reference[i - 1] != hypothesis[j - 1] {
                substitutions += 1;
            }
            i = i.saturating_sub(1);
            j = j.saturating_sub(1);
        }
    }
    (insertions, deletions, substitutions)
}

#[must_use]
pub fn percentile_nearest_rank(sorted: &[u64], p: u8) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p as usize) * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speech_case(id: &str, reference: &str, hypothesis: &str, pcm: Vec<u8>) -> BakeoffCase {
        BakeoffCase {
            id: id.into(),
            kind: CaseKind::Speech,
            split: Split::HeldOut,
            band: DurationBand::OneToTen,
            audio_hash: id.into(),
            reference: reference.into(),
            pcm,
            speech: Duration::from_millis(400),
            stratum: "pilot-quiet".into(),
            critical: Vec::new(),
            scripted_hypothesis: Some(hypothesis.into()),
        }
    }

    #[test]
    fn scoring_normalization_matches_the_locked_punctuation_set() {
        let wer = align_words("Hello, Raja!", "hello raja");
        assert_eq!(wer.reference_tokens, 2);
        assert_eq!(wer.insertions + wer.deletions + wer.substitutions, 0);
        assert_eq!(wer.error_rate, 0.0);
    }

    #[test]
    fn undersized_harness_corpus_does_not_satisfy_the_lock() {
        let cases = [speech_case("a", "one", "one", vec![1, 0])];
        assert!(!corpus_lock_satisfied(&cases));
        assert!(sample_counts(&cases).speech < MIN_SPEECH_RECORDINGS);
    }

    #[test]
    fn evaluate_report_never_selects_a_winner() {
        let cases = [speech_case("a", "one", "one", vec![1, 0])];
        let report = evaluate_report(
            RuntimeFamily::WhisperCppProcess,
            &cases,
            Vec::new(),
            vec!["no native whisper.cpp weights in CI".into()],
            None,
            false,
        );
        assert!(!report.winner_selected);
        assert!(!report.measurements_invented);
        assert!(!report.production_weights_downloaded);
        assert_eq!(report.verdict, GateVerdict::PendingEvidence);
        assert_eq!(report.corpus_version, CORPUS_VERSION);
    }

    #[test]
    fn empty_timing_slice_does_not_become_a_fake_pass() {
        assert_eq!(percentile_nearest_rank(&[], 95), None);
    }

    #[test]
    fn fedora_is_the_product_target_and_hyprland_is_a_pilot() {
        let profiles = locked_host_profiles();
        assert_eq!(profiles[0].role, HostRole::FirstSupportedProduct);
        assert_eq!(profiles[1].role, HostRole::PilotNotProductProof);
    }

    #[test]
    fn harness_runner_records_stop_anchored_timings_without_claiming_go() {
        let mut case = speech_case("rec-speech", "hello raja", "hello raja", vec![1, 0, 3, 0]);
        case.speech = Duration::from_millis(2_000);
        let mut runner = FeasibilityRunner::harness(FakeWorker::default());
        runner
            .prepare(Correlation {
                daemon_nonce: "bakeoff".into(),
                generation: 1,
                request_id: "prep".into(),
                recording_id: "prep".into(),
                model_receipt_hash: "harness-no-weights".into(),
            })
            .unwrap();
        let outcome = runner.run_case(&case).unwrap();
        let timings = outcome
            .timings
            .as_ref()
            .expect("fake runner still records clocks");
        assert!(
            (1_900..=2_100).contains(&timings.recording_duration_ms),
            "speech interval must stay on the Recording clock, got {}",
            timings.recording_duration_ms
        );
        assert!(
            timings
                .stop_to_delivered_ms
                .is_some_and(|ms| ms < timings.recording_duration_ms)
        );
        assert!(timings.stop_to_finalized_ms <= timings.stop_to_delivered_ms.unwrap_or(u64::MAX));
        assert_eq!(outcome.observed_device, "cpu");
        let report = evaluate_report(
            RuntimeFamily::WhisperCppProcess,
            std::slice::from_ref(&case),
            vec![outcome],
            vec!["harness smoke; locked corpus not present".into()],
            None,
            false,
        );
        assert_eq!(report.verdict, GateVerdict::PendingEvidence);
        assert!(!report.winner_selected);
    }

    #[test]
    fn negative_fixture_must_not_insert_text() {
        let case = BakeoffCase {
            id: "neg".into(),
            kind: CaseKind::Negative,
            split: Split::HeldOut,
            band: DurationBand::OneToTen,
            audio_hash: "neg".into(),
            reference: String::new(),
            pcm: vec![0, 0, 0, 0],
            speech: Duration::from_millis(200),
            stratum: "silence".into(),
            critical: Vec::new(),
            scripted_hypothesis: None,
        };
        let mut runner = FeasibilityRunner::harness(FakeWorker::default());
        runner
            .prepare(Correlation {
                daemon_nonce: "bakeoff".into(),
                generation: 1,
                request_id: "prep".into(),
                recording_id: "prep".into(),
                model_receipt_hash: "harness-no-weights".into(),
            })
            .unwrap();
        let outcome = runner.run_case(&case).unwrap();
        assert!(!outcome.delivered);
        assert!(outcome.hypothesis.is_none());
        assert!(outcome.critical_failures.is_empty());
    }

    #[test]
    fn critical_name_mismatch_is_recorded() {
        let mut case = speech_case("c", "call raja", "call alex", vec![1, 0]);
        case.critical = vec![(CriticalKind::Name, "raja".into())];
        let mut runner = FeasibilityRunner::harness(FakeWorker::default());
        runner
            .prepare(Correlation {
                daemon_nonce: "bakeoff".into(),
                generation: 1,
                request_id: "prep".into(),
                recording_id: "prep".into(),
                model_receipt_hash: "harness-no-weights".into(),
            })
            .unwrap();
        let outcome = runner.run_case(&case).unwrap();
        assert_eq!(outcome.critical_failures, vec!["Name:raja".to_owned()]);
    }

    #[test]
    fn critical_phrase_requires_contiguous_token_order() {
        assert!(!contains_contiguous_tokens(
            &tokenize("do delete, not save"),
            &tokenize("do not delete")
        ));
        assert!(contains_contiguous_tokens(
            &tokenize("please do not delete this"),
            &tokenize("do not delete")
        ));
        assert!(contains_contiguous_tokens(
            &tokenize("call raja"),
            &tokenize("raja")
        ));

        let mut scrambled = speech_case("p", "do not delete", "do delete, not save", vec![1, 0]);
        scrambled.critical = vec![(CriticalKind::RequiredPhrase, "do not delete".into())];
        let mut ordered = speech_case(
            "p2",
            "do not delete",
            "please do not delete this",
            vec![1, 0],
        );
        ordered.critical = vec![(CriticalKind::RequiredPhrase, "do not delete".into())];
        let mut runner = FeasibilityRunner::harness(FakeWorker::default());
        runner
            .prepare(Correlation {
                daemon_nonce: "bakeoff".into(),
                generation: 1,
                request_id: "prep".into(),
                recording_id: "prep".into(),
                model_receipt_hash: "harness-no-weights".into(),
            })
            .unwrap();
        let failed = runner.run_case(&scrambled).unwrap();
        assert_eq!(
            failed.critical_failures,
            vec!["RequiredPhrase:do not delete".to_owned()]
        );
        let passed = runner.run_case(&ordered).unwrap();
        assert!(passed.critical_failures.is_empty());
    }

    #[test]
    fn one_runner_uses_each_case_pcm_and_scripted_hypothesis() {
        let alpha = speech_case("alpha", "alpha", "alpha", vec![1, 0]);
        let bravo = speech_case("bravo", "bravo", "bravo", vec![3, 0]);
        let mut runner = FeasibilityRunner::harness(FakeWorker::default());
        runner
            .prepare(Correlation {
                daemon_nonce: "bakeoff".into(),
                generation: 1,
                request_id: "prep".into(),
                recording_id: "prep".into(),
                model_receipt_hash: "harness-no-weights".into(),
            })
            .unwrap();
        let first = runner.run_case(&alpha).unwrap();
        let second = runner.run_case(&bravo).unwrap();
        assert_eq!(first.hypothesis.as_deref(), Some("alpha"));
        assert_eq!(second.hypothesis.as_deref(), Some("bravo"));
    }

    #[test]
    fn locked_corpus_with_failing_wer_is_nogo() {
        let mut cases = Vec::new();
        for i in 0..100 {
            cases.push(BakeoffCase {
                id: format!("s{i}"),
                kind: CaseKind::Speech,
                split: if i < 20 {
                    Split::Tuning
                } else {
                    Split::HeldOut
                },
                band: if i < 5 {
                    DurationBand::OneTwentyToSixHundred
                } else {
                    DurationBand::OneToTen
                },
                audio_hash: format!("hs{i}"),
                reference: "hello raja".into(),
                pcm: vec![1, 0],
                speech: Duration::from_millis(200),
                stratum: "pilot-quiet".into(),
                critical: Vec::new(),
                scripted_hypothesis: Some("wrong".into()),
            });
        }
        for i in 0..20 {
            cases.push(BakeoffCase {
                id: format!("n{i}"),
                kind: CaseKind::Negative,
                split: Split::HeldOut,
                band: DurationBand::OneToTen,
                audio_hash: format!("hn{i}"),
                reference: String::new(),
                pcm: vec![0, 0],
                speech: Duration::from_millis(50),
                stratum: "silence".into(),
                critical: Vec::new(),
                scripted_hypothesis: None,
            });
        }
        assert!(corpus_lock_satisfied(&cases));
        let outcomes: Vec<CaseOutcome> = cases
            .iter()
            .map(|case| CaseOutcome {
                id: case.id.clone(),
                kind: case.kind,
                split: case.split,
                delivered: case.kind == CaseKind::Speech,
                hypothesis: case.scripted_hypothesis.clone(),
                wer: Some(align_words(&case.reference, "wrong")),
                critical_failures: Vec::new(),
                timings: None,
                observed_device: "cpu".into(),
                band: case.band,
                stratum: case.stratum.clone(),
            })
            .collect();
        let report = evaluate_report(
            RuntimeFamily::WhisperCppProcess,
            &cases,
            outcomes,
            Vec::new(),
            None,
            false,
        );
        assert_eq!(report.verdict, GateVerdict::NoGo);
        assert!(!report.winner_selected);
    }
}
