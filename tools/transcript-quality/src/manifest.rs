use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use voisu_core::RenderingPolicy;

/// On-disk JSON/JSONL of saved evaluation evidence.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub recordings: Vec<ManifestRecording>,
}

/// One Recording row in a manifest.
#[derive(Clone, Debug, Deserialize)]
pub struct ManifestRecording {
    pub correlation_id: String,
    #[serde(default)]
    pub audio_path: Option<String>,
    #[serde(default)]
    pub source_transcripts: SourceTranscriptFields,
    #[serde(default)]
    pub final_transcript: Option<String>,
    #[serde(default)]
    pub final_transcript_path: Option<String>,
    #[serde(default)]
    pub reference_path: Option<String>,
    #[serde(default)]
    pub reference_text: Option<String>,
    #[serde(default)]
    pub reference_kind: Option<String>,
    #[serde(default)]
    pub adjudicated: Option<bool>,
    #[serde(default)]
    pub speaker: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub rendering_policy: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SourceTranscriptFields {
    #[serde(default)]
    pub groq: Option<String>,
    #[serde(default)]
    pub deepgram: Option<String>,
    #[serde(default)]
    pub groq_path: Option<String>,
    #[serde(default)]
    pub deepgram_path: Option<String>,
}

/// Loaded, path-resolved evidence for one Recording.
#[derive(Clone, Debug)]
pub struct LoadedRecording {
    pub correlation_id: String,
    pub speaker: Option<String>,
    pub tags: Vec<String>,
    pub audio: EvidencePresence,
    pub groq: Option<String>,
    pub deepgram: Option<String>,
    pub final_transcript: Option<String>,
    pub reference: Option<String>,
    pub reference_missing_reason: Option<String>,
    pub rendering_policy: RenderingPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidencePresence {
    NotProvided,
    Present,
    Missing,
}

pub fn load_manifest(path: &Path) -> Result<Manifest, String> {
    let text = fs::read_to_string(path)
        .map_err(|err| format!("cannot read manifest {}: {err}", path.display()))?;
    parse_manifest_text(&text)
}

pub fn parse_manifest_text(text: &str) -> Result<Manifest, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("manifest is empty".to_owned());
    }
    if trimmed.starts_with('{') {
        let value: serde_json::Value =
            serde_json::from_str(trimmed).map_err(|err| format!("manifest JSON: {err}"))?;
        if value.get("recordings").is_some() {
            return serde_json::from_value(value)
                .map_err(|err| format!("manifest recordings: {err}"));
        }
        let recording: ManifestRecording =
            serde_json::from_value(value).map_err(|err| format!("manifest Recording: {err}"))?;
        return Ok(Manifest {
            recordings: vec![recording],
        });
    }
    if trimmed.starts_with('[') {
        let recordings: Vec<ManifestRecording> =
            serde_json::from_str(trimmed).map_err(|err| format!("manifest array: {err}"))?;
        return Ok(Manifest { recordings });
    }
    let mut recordings = Vec::new();
    for (idx, line) in trimmed.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let recording: ManifestRecording = serde_json::from_str(line)
            .map_err(|err| format!("manifest JSONL line {}: {err}", idx + 1))?;
        recordings.push(recording);
    }
    if recordings.is_empty() {
        return Err("manifest JSONL contained no Recordings".to_owned());
    }
    Ok(Manifest { recordings })
}

pub fn load_recordings(path: &Path) -> Result<Vec<LoadedRecording>, String> {
    let manifest = load_manifest(path)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    manifest
        .recordings
        .iter()
        .map(|row| resolve_recording(row, base))
        .collect()
}

fn resolve_recording(row: &ManifestRecording, base: &Path) -> Result<LoadedRecording, String> {
    if row.correlation_id.trim().is_empty() {
        return Err("Recording is missing correlation_id".to_owned());
    }
    let groq = load_optional_text(
        base,
        row.source_transcripts.groq.as_deref(),
        row.source_transcripts.groq_path.as_deref(),
    )?;
    let deepgram = load_optional_text(
        base,
        row.source_transcripts.deepgram.as_deref(),
        row.source_transcripts.deepgram_path.as_deref(),
    )?;
    let final_transcript = load_optional_text(
        base,
        row.final_transcript.as_deref(),
        row.final_transcript_path.as_deref(),
    )?;

    let audio = match row
        .audio_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => EvidencePresence::NotProvided,
        Some(raw) => {
            let path = resolve_path(base, raw);
            if path.is_file() {
                EvidencePresence::Present
            } else {
                EvidencePresence::Missing
            }
        }
    };

    let (reference, reference_missing_reason) = resolve_reference(row, base)?;
    let rendering_policy = parse_policy(row.rendering_policy.as_deref())?;

    Ok(LoadedRecording {
        correlation_id: row.correlation_id.clone(),
        speaker: row.speaker.clone(),
        tags: row.tags.clone(),
        audio,
        groq,
        deepgram,
        final_transcript,
        reference,
        reference_missing_reason,
        rendering_policy,
    })
}

fn resolve_reference(
    row: &ManifestRecording,
    base: &Path,
) -> Result<(Option<String>, Option<String>), String> {
    if reference_is_script(row) {
        return Ok((
            None,
            Some("reference is a reading script, not an audio-adjudicated reference".to_owned()),
        ));
    }
    if !reference_is_adjudicated(row) {
        let provided = row
            .reference_text
            .as_ref()
            .is_some_and(|t| !t.trim().is_empty())
            || row
                .reference_path
                .as_ref()
                .is_some_and(|p| !p.trim().is_empty());
        let reason = if provided {
            "reference is not marked adjudicated (set reference_kind to adjudicated or adjudicated=true)"
        } else {
            "no audio-adjudicated reference was provided"
        };
        return Ok((None, Some(reason.to_owned())));
    }
    let text = load_optional_text(
        base,
        row.reference_text.as_deref(),
        row.reference_path.as_deref(),
    )?;
    match text {
        Some(value) if !value.trim().is_empty() => Ok((Some(value), None)),
        Some(_) => Ok((None, Some("reference file is empty".to_owned()))),
        None if row.reference_path.is_some() => {
            Ok((None, Some("reference path is missing on disk".to_owned())))
        }
        None => Ok((
            None,
            Some("no audio-adjudicated reference was provided".to_owned()),
        )),
    }
}

fn reference_kind(row: &ManifestRecording) -> String {
    row.reference_kind
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .replace('-', "_")
}

fn reference_is_script(row: &ManifestRecording) -> bool {
    if matches!(
        reference_kind(row).as_str(),
        "script" | "reading_script" | "prompt" | "reading_prompt"
    ) {
        return true;
    }
    row.tags.iter().any(|tag| {
        let t = tag.trim().to_ascii_lowercase();
        t == "script" || t == "reading-script" || t == "reading_script"
    })
}

fn reference_is_adjudicated(row: &ManifestRecording) -> bool {
    if row.adjudicated == Some(false) {
        return false;
    }
    if row.adjudicated == Some(true) {
        return true;
    }
    matches!(
        reference_kind(row).as_str(),
        "adjudicated" | "audio_adjudicated" | "spoken"
    )
}

fn load_optional_text(
    base: &Path,
    inline: Option<&str>,
    path: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(path) = path.map(str::trim).filter(|s| !s.is_empty()) {
        let resolved = resolve_path(base, path);
        match fs::read_to_string(&resolved) {
            Ok(text) => Ok(Some(text)),
            Err(_) => Ok(None),
        }
    } else if let Some(inline) = inline {
        if inline.trim().is_empty() {
            Ok(None)
        } else {
            Ok(Some(inline.to_owned()))
        }
    } else {
        Ok(None)
    }
}

fn resolve_path(base: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn parse_policy(raw: Option<&str>) -> Result<RenderingPolicy, String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(RenderingPolicy::Adaptive),
        Some(value) => RenderingPolicy::parse(value).ok_or_else(|| {
            format!("unknown rendering_policy {value:?} (use natural, adaptive, or structured)")
        }),
    }
}
