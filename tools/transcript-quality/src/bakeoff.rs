use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const BAKEOFF_MANIFEST_SCHEMA: &str = "voisu-local-asr-bakeoff-manifest-v1";
pub const BAKEOFF_CONTRACT_JSON: &str = include_str!("../bakeoff-contract-v1.json");

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BakeoffContract {
    schema: String,
    id: String,
    corpus_requirements: CorpusRequirements,
    scoring: ScoringContract,
    latency_measurement: LatencyMeasurement,
    thresholds: Thresholds,
    public_report: PublicReportContract,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusRequirements {
    speech_clips_min: usize,
    real_speech_clips_min: usize,
    negative_clips_min: usize,
    tuning_speech_clips_exact: usize,
    held_out_speech_clips_min: usize,
    longest_band_speech_clips_min: usize,
    duration_bands: Vec<DurationBandContract>,
    negative_kinds: Vec<String>,
    required_coverage: Vec<String>,
    critical_content_categories: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurationBandContract {
    id: String,
    min_ms_inclusive: u64,
    max_ms_inclusive: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScoringContract {
    wer_normalization: WerNormalization,
    word_alignment: String,
    punctuation_scoring: PunctuationScoring,
    semantic_error_scoring: SemanticErrorScoring,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WerNormalization {
    id: String,
    tokenization: String,
    case_folding: String,
    unicode_normalization: String,
    boundary_marks_removed: Vec<String>,
    terminal_period_rule: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PunctuationScoring {
    id: String,
    marks: Vec<String>,
    alignment: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SemanticErrorScoring {
    id: String,
    categories: Vec<String>,
    aggregation: String,
    separate_from_wer: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LatencyMeasurement {
    clock: String,
    warm_start: String,
    warm_end: String,
    cold_start: String,
    cold_end: String,
    percentiles: Vec<String>,
    raw_local_timings_required: bool,
    averaged_totals_forbidden: bool,
    stages: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Thresholds {
    held_out_overall_wer_max: f64,
    held_out_stratum_wer_max: f64,
    critical_content_errors_max: usize,
    negative_fixture_insertions_max: usize,
    warm_p95_up_to_30_seconds_ms: u64,
    warm_p95_up_to_120_seconds_ms: u64,
    accepted_clip_stop_to_delivery_max_ms: u64,
    cold_readiness_max_ms: u64,
    soak_duration_min_ms: u64,
    soak_recordings_min: usize,
    soak_warmup_recordings: usize,
    rss_growth_max_bytes: u64,
    crashes_max: usize,
    duplicate_deliveries_max: usize,
    stale_responses_max: usize,
    unexpected_network_attempts_max: usize,
    unreaped_children_max: usize,
    full_length_completion_required: bool,
    packaged_runtime_required: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicReportContract {
    schema: String,
    fields: Vec<String>,
    forbidden_fields: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BakeoffManifest {
    schema: String,
    corpus_version: String,
    corpus_revision: String,
    contract: BakeoffContract,
    strata: Vec<Stratum>,
    host_profiles: Vec<HostProfile>,
    clips: Vec<Clip>,
    measurement_locks: Vec<MeasurementLock>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stratum {
    id: String,
    dimension: StratumDimension,
    label: String,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StratumDimension {
    Accent,
    Noise,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostProfile {
    id: String,
    os_release: String,
    kernel_release: String,
    architecture: String,
    cpu: String,
    accelerator: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Clip {
    id: String,
    kind: ClipKind,
    #[serde(default)]
    negative_kind: Option<NegativeKind>,
    audio_sha256: String,
    #[serde(default)]
    reference_sha256: Option<String>,
    split: Split,
    duration_ms: u64,
    #[serde(default)]
    duration_band: Option<DurationBand>,
    #[serde(default)]
    language: Option<LanguageDeclaration>,
    #[serde(default)]
    accent_stratum: Option<String>,
    #[serde(default)]
    noise_stratum: Option<String>,
    coverage: Vec<String>,
    critical_content: Vec<CriticalCategory>,
    provenance: Provenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClipKind {
    Speech,
    Negative,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NegativeKind {
    Silence,
    Noise,
    NonSpeech,
}

impl NegativeKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Silence => "silence",
            Self::Noise => "noise",
            Self::NonSpeech => "non_speech",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Split {
    Tuning,
    HeldOut,
}

impl Split {
    fn as_str(self) -> &'static str {
        match self {
            Self::Tuning => "tuning",
            Self::HeldOut => "held_out",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DurationBand {
    #[serde(rename = "one_to_ten_seconds")]
    OneToTen,
    #[serde(rename = "ten_to_thirty_seconds")]
    TenToThirty,
    #[serde(rename = "thirty_to_one_twenty_seconds")]
    ThirtyToOneTwenty,
    #[serde(rename = "one_twenty_to_six_hundred_seconds")]
    OneTwentyToSixHundred,
}

impl DurationBand {
    fn as_str(self) -> &'static str {
        match self {
            Self::OneToTen => "one_to_ten_seconds",
            Self::TenToThirty => "ten_to_thirty_seconds",
            Self::ThirtyToOneTwenty => "thirty_to_one_twenty_seconds",
            Self::OneTwentyToSixHundred => "one_twenty_to_six_hundred_seconds",
        }
    }

    fn contains(self, duration_ms: u64) -> bool {
        match self {
            Self::OneToTen => (1_000..=10_000).contains(&duration_ms),
            Self::TenToThirty => (10_001..=30_000).contains(&duration_ms),
            Self::ThirtyToOneTwenty => (30_001..=120_000).contains(&duration_ms),
            Self::OneTwentyToSixHundred => (120_001..=600_000).contains(&duration_ms),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LanguageDeclaration {
    tag: String,
    supported: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CriticalCategory {
    Number,
    Name,
    Negation,
    Command,
    Path,
    Url,
    Unit,
    OmittedPhrase,
}

impl CriticalCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Name => "name",
            Self::Negation => "negation",
            Self::Command => "command",
            Self::Path => "path",
            Self::Url => "url",
            Self::Unit => "unit",
            Self::OmittedPhrase => "omitted_phrase",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Provenance {
    source_kind: SourceKind,
    source_id: String,
    source_revision: String,
    consent_basis: ConsentBasis,
    #[serde(default)]
    license: Option<String>,
    redistribution: Redistribution,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SourceKind {
    PublicDataset,
    PrivateRecording,
    Synthetic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConsentBasis {
    Explicit,
    DatasetLicense,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Redistribution {
    Public,
    MetadataOnly,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MeasurementLock {
    measurement_id: String,
    manifest_sha256: String,
}

#[derive(Serialize)]
struct LockMaterial<'a> {
    schema: &'a str,
    corpus_version: &'a str,
    corpus_revision: &'a str,
    contract: &'a BakeoffContract,
    strata: &'a [Stratum],
    host_profiles: &'a [HostProfile],
    clips: &'a [Clip],
}

#[derive(Clone, Debug)]
pub struct ValidatedBakeoffManifest {
    pub speech_clips: usize,
    pub real_speech_clips: usize,
    pub tuning_speech_clips: usize,
    pub held_out_speech_clips: usize,
    pub negative_clips: usize,
    pub longest_band_speech_clips: usize,
    pub manifest_sha256: String,
    pub public_summary: Value,
}

/// Local-only file inputs used to verify the manifest's claimed content hashes.
/// Paths and file contents never enter the public summary.
pub struct BakeoffHashInput {
    pub clip_id: String,
    pub audio_path: PathBuf,
    pub reference_path: Option<PathBuf>,
}

pub fn load_and_validate_bakeoff_manifest(path: &Path) -> Result<ValidatedBakeoffManifest, String> {
    let text = fs::read_to_string(path)
        .map_err(|err| format!("cannot read bakeoff manifest {}: {err}", path.display()))?;
    validate_bakeoff_manifest_text(&text)
}

pub fn validate_bakeoff_manifest_text(text: &str) -> Result<ValidatedBakeoffManifest, String> {
    validate_bakeoff_manifest_text_with_expected_hash(text, None)
}

/// Validates a manifest against an externally retained measurement attestation.
/// The expected hash must live outside the mutable manifest for this gate to be
/// meaningful.
pub fn validate_bakeoff_manifest_text_for_measurement(
    text: &str,
    expected_manifest_sha256: &str,
) -> Result<ValidatedBakeoffManifest, String> {
    validate_bakeoff_manifest_text_with_expected_hash(text, Some(expected_manifest_sha256))
}

pub fn load_and_validate_bakeoff_manifest_for_measurement(
    path: &Path,
    expected_manifest_sha256: &str,
) -> Result<ValidatedBakeoffManifest, String> {
    let text = fs::read_to_string(path)
        .map_err(|err| format!("cannot read bakeoff manifest {}: {err}", path.display()))?;
    validate_bakeoff_manifest_text_for_measurement(&text, expected_manifest_sha256)
}

fn validate_bakeoff_manifest_text_with_expected_hash(
    text: &str,
    expected_manifest_sha256: Option<&str>,
) -> Result<ValidatedBakeoffManifest, String> {
    let manifest: BakeoffManifest =
        serde_json::from_str(text).map_err(|err| format!("bakeoff manifest JSON: {err}"))?;
    let frozen: BakeoffContract = serde_json::from_str(BAKEOFF_CONTRACT_JSON)
        .map_err(|err| format!("built-in bakeoff contract is invalid: {err}"))?;
    if manifest.schema != BAKEOFF_MANIFEST_SCHEMA {
        return Err(format!(
            "bakeoff manifest schema {:?} is not {BAKEOFF_MANIFEST_SCHEMA:?}",
            manifest.schema
        ));
    }
    if manifest.contract != frozen {
        return Err(format!(
            "manifest contract differs from the locked contract {}; threshold or scoring edits require a new version before measurement",
            frozen.id
        ));
    }
    require_text("corpus_version", &manifest.corpus_version)?;
    require_text("corpus_revision", &manifest.corpus_revision)?;
    validate_host_profiles(&manifest.host_profiles)?;
    let strata = validate_strata(&manifest.strata)?;

    let mut ids = BTreeSet::new();
    let mut hashes: BTreeMap<&str, Split> = BTreeMap::new();
    let mut speech = 0usize;
    let mut real_speech = 0usize;
    let mut tuning_speech = 0usize;
    let mut held_out_speech = 0usize;
    let mut negatives = 0usize;
    let mut duration_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut negative_kinds = BTreeSet::new();
    let mut held_out_coverage: BTreeSet<String> = BTreeSet::new();
    let mut held_out_critical: BTreeSet<String> = BTreeSet::new();
    let mut held_out_strata: BTreeSet<String> = BTreeSet::new();
    let mut has_public_speech = false;
    let mut has_private_speech = false;

    for clip in &manifest.clips {
        require_id("clip id", &clip.id)?;
        if !ids.insert(clip.id.as_str()) {
            return Err(format!("duplicate clip id {:?}", clip.id));
        }
        validate_hash("audio_sha256", &clip.audio_sha256, &clip.id)?;
        if let Some(previous_split) = hashes.insert(&clip.audio_sha256, clip.split) {
            if previous_split != clip.split {
                return Err(format!(
                    "split leakage: audio_sha256 {} appears in both {} and {}",
                    clip.audio_sha256,
                    previous_split.as_str(),
                    clip.split.as_str()
                ));
            }
            return Err(format!(
                "duplicate audio_sha256 {} in split {}",
                clip.audio_sha256,
                clip.split.as_str()
            ));
        }
        validate_provenance(&clip.provenance, &clip.id)?;
        match clip.kind {
            ClipKind::Speech => {
                speech += 1;
                match clip.split {
                    Split::Tuning => tuning_speech += 1,
                    Split::HeldOut => held_out_speech += 1,
                }
                validate_speech_clip(
                    clip,
                    &strata,
                    &mut duration_counts,
                    &mut held_out_coverage,
                    &mut held_out_critical,
                    &mut held_out_strata,
                )?;
                match clip.provenance.source_kind {
                    SourceKind::PublicDataset => {
                        has_public_speech = true;
                        real_speech += 1;
                    }
                    SourceKind::PrivateRecording => {
                        has_private_speech = true;
                        real_speech += 1;
                    }
                    SourceKind::Synthetic => {}
                }
            }
            ClipKind::Negative => {
                negatives += 1;
                validate_negative_clip(clip, &mut negative_kinds)?;
            }
        }
    }

    let requirements = &frozen.corpus_requirements;
    require_minimum("speech clips", speech, requirements.speech_clips_min)?;
    require_minimum(
        "real speech clips",
        real_speech,
        requirements.real_speech_clips_min,
    )?;
    require_minimum(
        "held-out speech clips",
        held_out_speech,
        requirements.held_out_speech_clips_min,
    )?;
    require_minimum("negative clips", negatives, requirements.negative_clips_min)?;
    if tuning_speech != requirements.tuning_speech_clips_exact {
        return Err(format!(
            "tuning speech clips must equal {}, found {tuning_speech}",
            requirements.tuning_speech_clips_exact
        ));
    }
    for band in &requirements.duration_bands {
        let count = duration_counts.get(band.id.as_str()).copied().unwrap_or(0);
        if count == 0 {
            return Err(format!("duration stratum {} has no speech clips", band.id));
        }
    }
    let longest_band = DurationBand::OneTwentyToSixHundred.as_str();
    let longest_count = duration_counts.get(longest_band).copied().unwrap_or(0);
    require_minimum(
        "longest duration-band speech clips",
        longest_count,
        requirements.longest_band_speech_clips_min,
    )?;
    for kind in &requirements.negative_kinds {
        if !negative_kinds.contains(kind.as_str()) {
            return Err(format!("negative fixtures do not cover {kind}"));
        }
    }
    for coverage in &requirements.required_coverage {
        if !held_out_coverage.contains(coverage.as_str()) {
            return Err(format!(
                "held-out speech does not cover required tag {coverage}"
            ));
        }
    }
    for category in &requirements.critical_content_categories {
        if !held_out_critical.contains(category.as_str()) {
            return Err(format!(
                "held-out speech does not declare critical-content category {category}"
            ));
        }
    }
    for stratum in strata.keys() {
        if !held_out_strata.contains(stratum.as_str()) {
            return Err(format!(
                "declared stratum {stratum:?} has no held-out speech clip"
            ));
        }
    }
    if !has_public_speech {
        return Err("speech corpus requires at least one public licensed speech source".to_owned());
    }
    if !has_private_speech {
        return Err(
            "speech corpus requires at least one explicitly consented private metadata-only speech source".to_owned(),
        );
    }

    let lock_material = LockMaterial {
        schema: &manifest.schema,
        corpus_version: &manifest.corpus_version,
        corpus_revision: &manifest.corpus_revision,
        contract: &manifest.contract,
        strata: &manifest.strata,
        host_profiles: &manifest.host_profiles,
        clips: &manifest.clips,
    };
    let manifest_sha256 = sha256_json(&lock_material)?;
    validate_measurement_locks(
        &manifest.measurement_locks,
        &manifest_sha256,
        expected_manifest_sha256,
    )?;
    let contract_sha256 = sha256_bytes(BAKEOFF_CONTRACT_JSON.as_bytes());
    let public_summary = json!({
        "schema": "voisu-local-asr-bakeoff-manifest-summary-v1",
        "contract_id": frozen.id,
        "contract_sha256": contract_sha256,
        "manifest_sha256": manifest_sha256,
        "corpus_version": manifest.corpus_version,
        "corpus_revision": manifest.corpus_revision,
        "sample_counts": {
            "speech": speech,
            "real_speech": real_speech,
            "tuning_speech": tuning_speech,
            "held_out_speech": held_out_speech,
            "negative": negatives
        },
        "duration_band_counts": duration_counts,
        "strata": manifest.strata.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
        "host_profiles": manifest.host_profiles.iter().map(|item| item.id.as_str()).collect::<Vec<_>>()
    });
    Ok(ValidatedBakeoffManifest {
        speech_clips: speech,
        real_speech_clips: real_speech,
        tuning_speech_clips: tuning_speech,
        held_out_speech_clips: held_out_speech,
        negative_clips: negatives,
        longest_band_speech_clips: longest_count,
        manifest_sha256,
        public_summary,
    })
}

/// Hashes the exact local audio and reference files named by the caller and
/// compares them with the manifest. The caller owns the private path mapping;
/// this API returns only the same aggregate metadata summary as validation.
pub fn verify_bakeoff_hash_inputs(
    text: &str,
    inputs: &[BakeoffHashInput],
) -> Result<ValidatedBakeoffManifest, String> {
    let validated = validate_bakeoff_manifest_text(text)?;
    let manifest: BakeoffManifest =
        serde_json::from_str(text).map_err(|err| format!("bakeoff manifest JSON: {err}"))?;
    let clips: BTreeMap<&str, &Clip> = manifest
        .clips
        .iter()
        .map(|clip| (clip.id.as_str(), clip))
        .collect();
    let mut seen = BTreeSet::new();
    for input in inputs {
        if !seen.insert(input.clip_id.as_str()) {
            return Err(format!("duplicate hash input for clip {:?}", input.clip_id));
        }
        let clip = clips
            .get(input.clip_id.as_str())
            .ok_or_else(|| format!("hash input names unknown clip {:?}", input.clip_id))?;
        let audio_sha256 = sha256_file(&input.audio_path, "audio", &input.clip_id)?;
        if audio_sha256 != clip.audio_sha256 {
            return Err(format!(
                "clip {} audio hash is {}, expected {}",
                input.clip_id, audio_sha256, clip.audio_sha256
            ));
        }
        match (
            clip.kind,
            input.reference_path.as_ref(),
            clip.reference_sha256.as_deref(),
        ) {
            (ClipKind::Speech, Some(reference_path), Some(expected)) => {
                let reference_sha256 = sha256_file(reference_path, "reference", &input.clip_id)?;
                if reference_sha256 != expected {
                    return Err(format!(
                        "clip {} reference hash is {}, expected {}",
                        input.clip_id, reference_sha256, expected
                    ));
                }
            }
            (ClipKind::Speech, None, _) => {
                return Err(format!(
                    "speech clip {} is missing a local reference hash input",
                    input.clip_id
                ));
            }
            (ClipKind::Negative, Some(_), _) => {
                return Err(format!(
                    "negative clip {} must not have a reference hash input",
                    input.clip_id
                ));
            }
            (ClipKind::Negative, None, None) => {}
            (ClipKind::Negative, None, Some(_)) => {
                return Err(format!(
                    "negative clip {} unexpectedly declares a reference hash",
                    input.clip_id
                ));
            }
            (ClipKind::Speech, Some(_), None) => {
                return Err(format!(
                    "speech clip {} is missing reference_sha256",
                    input.clip_id
                ));
            }
        }
    }
    if seen.len() != clips.len() {
        let missing: Vec<&str> = clips
            .keys()
            .copied()
            .filter(|id| !seen.contains(id))
            .collect();
        return Err(format!("missing local hash inputs for clips {missing:?}"));
    }
    Ok(validated)
}

fn sha256_file(path: &Path, kind: &str, clip_id: &str) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|err| format!("cannot inspect {kind} file for clip {clip_id}: {err}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{kind} file for clip {clip_id} must be a regular non-symlink file"
        ));
    }
    let bytes = fs::read(path)
        .map_err(|err| format!("cannot read {kind} file for clip {clip_id}: {err}"))?;
    Ok(sha256_bytes(&bytes))
}

fn validate_host_profiles(profiles: &[HostProfile]) -> Result<(), String> {
    if profiles.is_empty() {
        return Err("at least one frozen host profile is required".to_owned());
    }
    let mut ids = BTreeSet::new();
    for profile in profiles {
        require_id("host profile id", &profile.id)?;
        if !ids.insert(profile.id.as_str()) {
            return Err(format!("duplicate host profile id {:?}", profile.id));
        }
        for (field, value) in [
            ("os_release", &profile.os_release),
            ("kernel_release", &profile.kernel_release),
            ("architecture", &profile.architecture),
            ("cpu", &profile.cpu),
            ("accelerator", &profile.accelerator),
        ] {
            require_text(&format!("host profile {} {field}", profile.id), value)?;
        }
    }
    Ok(())
}

fn validate_strata(strata: &[Stratum]) -> Result<BTreeMap<String, StratumDimension>, String> {
    let mut declared = BTreeMap::new();
    for stratum in strata {
        require_id("stratum id", &stratum.id)?;
        require_text("stratum label", &stratum.label)?;
        if declared
            .insert(stratum.id.clone(), stratum.dimension)
            .is_some()
        {
            return Err(format!("duplicate stratum id {:?}", stratum.id));
        }
    }
    if !declared
        .values()
        .any(|value| *value == StratumDimension::Accent)
    {
        return Err("at least one accent stratum is required".to_owned());
    }
    if !declared
        .values()
        .any(|value| *value == StratumDimension::Noise)
    {
        return Err("at least one noise stratum is required".to_owned());
    }
    Ok(declared)
}

fn validate_speech_clip(
    clip: &Clip,
    strata: &BTreeMap<String, StratumDimension>,
    duration_counts: &mut BTreeMap<&'static str, usize>,
    held_out_coverage: &mut BTreeSet<String>,
    held_out_critical: &mut BTreeSet<String>,
    held_out_strata: &mut BTreeSet<String>,
) -> Result<(), String> {
    if clip.negative_kind.is_some() {
        return Err(format!("speech clip {} declares negative_kind", clip.id));
    }
    let reference = clip
        .reference_sha256
        .as_deref()
        .ok_or_else(|| format!("speech clip {} is missing reference_sha256", clip.id))?;
    validate_hash("reference_sha256", reference, &clip.id)?;
    let language = clip
        .language
        .as_ref()
        .ok_or_else(|| format!("speech clip {} is missing language", clip.id))?;
    if language.tag != "en" || !language.supported {
        return Err(format!(
            "speech clip {} must declare supported English (tag en)",
            clip.id
        ));
    }
    let band = clip
        .duration_band
        .ok_or_else(|| format!("speech clip {} is missing duration_band", clip.id))?;
    if !band.contains(clip.duration_ms) {
        return Err(format!(
            "speech clip {} duration {}ms does not fit declared band {}",
            clip.id,
            clip.duration_ms,
            band.as_str()
        ));
    }
    *duration_counts.entry(band.as_str()).or_default() += 1;
    let accent = required_stratum(
        clip,
        clip.accent_stratum.as_deref(),
        StratumDimension::Accent,
        strata,
    )?;
    let noise = required_stratum(
        clip,
        clip.noise_stratum.as_deref(),
        StratumDimension::Noise,
        strata,
    )?;
    if clip.split == Split::HeldOut {
        held_out_strata.insert(accent.to_owned());
        held_out_strata.insert(noise.to_owned());
        held_out_coverage.extend(clip.coverage.iter().cloned());
        held_out_critical.extend(
            clip.critical_content
                .iter()
                .map(|value| value.as_str().to_owned()),
        );
    }
    Ok(())
}

fn required_stratum<'a>(
    clip: &Clip,
    id: Option<&'a str>,
    expected: StratumDimension,
    strata: &BTreeMap<String, StratumDimension>,
) -> Result<&'a str, String> {
    let id = id.ok_or_else(|| {
        format!(
            "speech clip {} is missing {} stratum",
            clip.id,
            match expected {
                StratumDimension::Accent => "accent",
                StratumDimension::Noise => "noise",
            }
        )
    })?;
    match strata.get(id) {
        Some(actual) if *actual == expected => Ok(id),
        Some(_) => Err(format!(
            "speech clip {} uses stratum {id:?} for the wrong dimension",
            clip.id
        )),
        None => Err(format!(
            "speech clip {} uses undeclared stratum {id:?}",
            clip.id
        )),
    }
}

fn validate_negative_clip(clip: &Clip, negative_kinds: &mut BTreeSet<&str>) -> Result<(), String> {
    if clip.split != Split::HeldOut {
        return Err(format!("negative clip {} must be held_out", clip.id));
    }
    let kind = clip
        .negative_kind
        .ok_or_else(|| format!("negative clip {} is missing negative_kind", clip.id))?;
    negative_kinds.insert(kind.as_str());
    if clip.reference_sha256.is_some()
        || clip.duration_band.is_some()
        || clip.language.is_some()
        || clip.accent_stratum.is_some()
        || clip.noise_stratum.is_some()
        || !clip.critical_content.is_empty()
    {
        return Err(format!(
            "negative clip {} must not carry speech reference, language, stratum, duration-band, or critical-content fields",
            clip.id
        ));
    }
    if clip.duration_ms == 0 || clip.duration_ms > 600_000 {
        return Err(format!(
            "negative clip {} duration must be 1..=600000ms",
            clip.id
        ));
    }
    Ok(())
}

fn validate_provenance(provenance: &Provenance, clip_id: &str) -> Result<(), String> {
    require_text(
        &format!("clip {clip_id} provenance source_id"),
        &provenance.source_id,
    )?;
    require_text(
        &format!("clip {clip_id} provenance source_revision"),
        &provenance.source_revision,
    )?;
    match (provenance.source_kind, provenance.consent_basis) {
        (SourceKind::PublicDataset, ConsentBasis::DatasetLicense) => {
            let license = provenance.license.as_deref().unwrap_or("");
            require_text(&format!("clip {clip_id} provenance license"), license)?;
            if provenance.redistribution != Redistribution::Public {
                return Err(format!(
                    "public dataset clip {clip_id} must set redistribution to public"
                ));
            }
        }
        (SourceKind::PrivateRecording, ConsentBasis::Explicit)
        | (SourceKind::Synthetic, ConsentBasis::Explicit) => {}
        _ => {
            return Err(format!(
                "clip {clip_id} provenance consent_basis does not match source_kind"
            ));
        }
    }
    if provenance.source_kind == SourceKind::PrivateRecording
        && provenance.redistribution != Redistribution::MetadataOnly
    {
        return Err(format!(
            "private Recording clip {clip_id} must set redistribution to metadata_only"
        ));
    }
    Ok(())
}

fn validate_measurement_locks(
    locks: &[MeasurementLock],
    manifest_sha256: &str,
    expected_manifest_sha256: Option<&str>,
) -> Result<(), String> {
    if let Some(expected) = expected_manifest_sha256 {
        validate_hash(
            "expected_manifest_sha256",
            expected,
            "measurement attestation",
        )?;
        if expected != manifest_sha256 {
            return Err(format!(
                "external measurement attestation records manifest {expected}, but the current contract and corpus metadata hash to {manifest_sha256}; rejecting post-measurement edit"
            ));
        }
        if locks.is_empty() {
            return Err(
                "measured validation requires a measurement lock matching the external attestation"
                    .to_owned(),
            );
        }
    }
    let mut ids = BTreeSet::new();
    for lock in locks {
        require_id("measurement_id", &lock.measurement_id)?;
        if !ids.insert(lock.measurement_id.as_str()) {
            return Err(format!(
                "duplicate measurement_id {:?}",
                lock.measurement_id
            ));
        }
        validate_hash(
            "manifest_sha256",
            &lock.manifest_sha256,
            &lock.measurement_id,
        )?;
        if lock.manifest_sha256 != manifest_sha256 {
            return Err(format!(
                "measurement {} records manifest {}, but the current contract and corpus metadata hash to {}; rejecting post-measurement edit",
                lock.measurement_id, lock.manifest_sha256, manifest_sha256
            ));
        }
    }
    Ok(())
}

fn require_minimum(label: &str, actual: usize, minimum: usize) -> Result<(), String> {
    if actual < minimum {
        Err(format!(
            "{label} require at least {minimum}, found {actual}"
        ))
    } else {
        Ok(())
    }
}

fn require_text(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{field} must not be empty"))
    } else {
        Ok(())
    }
}

fn require_id(field: &str, value: &str) -> Result<(), String> {
    require_text(field, value)?;
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Ok(())
    } else {
        Err(format!(
            "{field} {value:?} must use only ASCII letters, digits, '-', '_', or '.'"
        ))
    }
}

fn validate_hash(field: &str, value: &str, owner: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(format!(
            "{owner}: {field} must be exactly 64 lowercase hexadecimal characters"
        ))
    }
}

fn sha256_json(value: &impl Serialize) -> Result<String, String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|err| format!("cannot serialize bakeoff manifest lock: {err}"))?;
    Ok(sha256_bytes(&bytes))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
