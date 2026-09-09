use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use transcript_quality::{
    BAKEOFF_CONTRACT_JSON, BakeoffHashInput, align_punctuation, validate_bakeoff_manifest_text,
    validate_bakeoff_manifest_text_for_measurement, verify_bakeoff_hash_inputs,
};

const CRITICAL_CATEGORIES: [&str; 8] = [
    "number",
    "name",
    "negation",
    "command",
    "path",
    "url",
    "unit",
    "omitted_phrase",
];

fn hash(index: usize) -> String {
    format!("{index:064x}")
}

fn speech_clip(index: usize, split: &str) -> Value {
    let (duration_ms, duration_band) = match index % 4 {
        0 => (5_000, "one_to_ten_seconds"),
        1 => (20_000, "ten_to_thirty_seconds"),
        2 => (60_000, "thirty_to_one_twenty_seconds"),
        _ => (180_000, "one_twenty_to_six_hundred_seconds"),
    };
    let (source_kind, consent_basis, license, redistribution) = match index {
        0 => (
            "public_dataset",
            "dataset_license",
            Some("CC-BY-4.0"),
            "public",
        ),
        1 => ("private_recording", "explicit", None, "metadata_only"),
        _ => (
            "public_dataset",
            "dataset_license",
            Some("CC-BY-4.0"),
            "public",
        ),
    };
    let mut provenance = json!({
        "source_kind": source_kind,
        "source_id": if index == 1 { "raja-private-recording" } else { "public-corpus" },
        "source_revision": "v1",
        "consent_basis": consent_basis,
        "redistribution": redistribution
    });
    if let Some(license) = license {
        provenance["license"] = json!(license);
    }
    json!({
        "id": format!("speech-{index:03}"),
        "kind": "speech",
        "audio_sha256": hash(index + 1),
        "reference_sha256": hash(index + 10_000),
        "split": split,
        "duration_ms": duration_ms,
        "duration_band": duration_band,
        "language": { "tag": "en", "supported": true },
        "accent_stratum": if index.is_multiple_of(2) { "pilot-accent" } else { "other-english" },
        "noise_stratum": if index.is_multiple_of(3) { "ambient" } else { "quiet" },
        "coverage": ["technical_vocabulary", "self_correction", "deterministic_formatting"],
        "critical_content": [CRITICAL_CATEGORIES[index % CRITICAL_CATEGORIES.len()]],
        "provenance": provenance
    })
}

fn negative_clip(index: usize) -> Value {
    json!({
        "id": format!("negative-{index:03}"),
        "kind": "negative",
        "negative_kind": match index % 3 {
            0 => "silence",
            1 => "noise",
            _ => "non_speech",
        },
        "audio_sha256": hash(index + 20_000),
        "split": "held_out",
        "duration_ms": 5_000,
        "coverage": [],
        "critical_content": [],
        "provenance": {
            "source_kind": "synthetic",
            "source_id": "hermetic-negative-fixture",
            "source_revision": "v1",
            "consent_basis": "explicit",
            "license": "CC0-1.0",
            "redistribution": "public"
        }
    })
}

fn valid_manifest() -> Value {
    let mut clips = Vec::new();
    for index in 0..100 {
        clips.push(speech_clip(
            index,
            if index < 20 { "tuning" } else { "held_out" },
        ));
    }
    for index in 0..20 {
        clips.push(negative_clip(index));
    }
    json!({
        "schema": "voisu-local-asr-bakeoff-manifest-v1",
        "corpus_version": "real-provenance-fixture-r7-v1",
        "corpus_revision": "fixture-revision-1",
        "contract": serde_json::from_str::<Value>(BAKEOFF_CONTRACT_JSON).unwrap(),
        "strata": [
            { "id": "pilot-accent", "dimension": "accent", "label": "Pilot accent" },
            { "id": "other-english", "dimension": "accent", "label": "Other English" },
            { "id": "quiet", "dimension": "noise", "label": "Quiet" },
            { "id": "ambient", "dimension": "noise", "label": "Ambient noise" }
        ],
        "host_profiles": [{
            "id": "synthetic-host",
            "os_release": "Synthetic Linux 1",
            "kernel_release": "test-kernel",
            "architecture": "x86_64",
            "cpu": "synthetic-cpu",
            "accelerator": "cpu"
        }],
        "clips": clips,
        "measurement_locks": []
    })
}

fn validation_error(manifest: &Value) -> String {
    validate_bakeoff_manifest_text(&serde_json::to_string(manifest).unwrap()).unwrap_err()
}

#[test]
fn valid_mixed_provenance_metadata_freezes_the_r7_contract() {
    let validated = validate_bakeoff_manifest_text(
        &serde_json::to_string(&valid_manifest()).expect("serialize manifest"),
    )
    .expect("valid R7 manifest");

    assert_eq!(validated.speech_clips, 100);
    assert_eq!(validated.real_speech_clips, 100);
    assert_eq!(validated.tuning_speech_clips, 20);
    assert_eq!(validated.held_out_speech_clips, 80);
    assert_eq!(validated.negative_clips, 20);
    assert_eq!(validated.longest_band_speech_clips, 25);
    assert_eq!(validated.manifest_sha256.len(), 64);
    assert_eq!(
        validated.public_summary["corpus_version"],
        "real-provenance-fixture-r7-v1"
    );
    assert!(validated.public_summary.get("clips").is_none());
}

#[test]
fn synthetic_speech_does_not_satisfy_real_speech_minimum() {
    for real_speech_count in [0, 2] {
        let mut manifest = valid_manifest();
        for (index, clip) in manifest["clips"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .take(100)
            .enumerate()
        {
            if index >= real_speech_count {
                clip["provenance"]["source_kind"] = json!("synthetic");
                clip["provenance"]["consent_basis"] = json!("explicit");
            }
        }
        let err = validation_error(&manifest);
        assert!(err.contains("real speech clips"), "{err}");
    }
}

#[test]
fn wer_and_punctuation_are_separate_scores() {
    let word = transcript_quality::align_words("Deploy now.", "deploy now!");
    let punctuation = align_punctuation("Deploy now.", "deploy now!");

    assert_eq!(word.error_rate, 0.0);
    assert_eq!(punctuation.reference_marks, 1);
    assert_eq!(punctuation.substitutions, 1);
    assert_eq!(punctuation.error_rate, 1.0);
    assert!(transcript_quality::detect_critical_errors("Deploy now.", "deploy now!").is_empty());
}

#[test]
fn rejects_duplicate_hashes_and_split_leakage() {
    let mut duplicate = valid_manifest();
    duplicate["clips"][21]["audio_sha256"] = duplicate["clips"][20]["audio_sha256"].clone();
    let err = validation_error(&duplicate);
    assert!(err.contains("duplicate audio_sha256"), "{err}");

    let mut leakage = valid_manifest();
    let mut leaked = leakage["clips"][0].clone();
    leaked["id"] = json!("leaked-copy");
    leaked["split"] = json!("held_out");
    leakage["clips"].as_array_mut().unwrap().push(leaked);
    let err = validation_error(&leakage);
    assert!(err.contains("split leakage"), "{err}");
}

#[test]
fn rejects_missing_references_and_insufficient_duration_strata() {
    let mut missing_reference = valid_manifest();
    missing_reference["clips"][0]
        .as_object_mut()
        .unwrap()
        .remove("reference_sha256");
    let err = validation_error(&missing_reference);
    assert!(err.contains("reference_sha256"), "{err}");

    let mut missing_band = valid_manifest();
    for clip in missing_band["clips"].as_array_mut().unwrap() {
        if clip["duration_band"] == "thirty_to_one_twenty_seconds" {
            clip["duration_ms"] = json!(20_000);
            clip["duration_band"] = json!("ten_to_thirty_seconds");
        }
    }
    let err = validation_error(&missing_band);
    assert!(err.contains("thirty_to_one_twenty_seconds"), "{err}");
}

#[test]
fn rejects_malformed_or_incomplete_declarations() {
    let err = validate_bakeoff_manifest_text("{not json").unwrap_err();
    assert!(err.contains("manifest JSON"), "{err}");

    let mut unknown = valid_manifest();
    unknown["surprise"] = json!(true);
    let err = validation_error(&unknown);
    assert!(err.contains("unknown field"), "{err}");

    let mut missing_category = valid_manifest();
    for clip in missing_category["clips"].as_array_mut().unwrap() {
        if let Some(categories) = clip["critical_content"].as_array_mut() {
            categories.retain(|category| category != "url");
        }
    }
    let err = validation_error(&missing_category);
    assert!(err.contains("critical-content category url"), "{err}");
}

#[test]
fn rejects_threshold_edits_and_changes_after_measurement_starts() {
    let mut manifest = valid_manifest();
    let first = validate_bakeoff_manifest_text(&serde_json::to_string(&manifest).unwrap()).unwrap();
    let attested_hash = first.manifest_sha256.clone();
    manifest["measurement_locks"] = json!([{
        "measurement_id": "synthetic-run-1",
        "manifest_sha256": attested_hash
    }]);
    validate_bakeoff_manifest_text_for_measurement(
        &serde_json::to_string(&manifest).unwrap(),
        &first.manifest_sha256,
    )
    .expect("unchanged measured manifest");
    let measured_manifest = manifest.clone();

    manifest["contract"]["thresholds"]["held_out_overall_wer_max"] = json!(0.11);
    let err = validation_error(&manifest);
    assert!(
        err.contains("locked contract") || err.contains("threshold"),
        "{err}"
    );

    let mut post_measurement_edit = measured_manifest.clone();
    post_measurement_edit["measurement_locks"] = json!([{
        "measurement_id": "synthetic-run-1",
        "manifest_sha256": first.manifest_sha256
    }]);
    post_measurement_edit["clips"][0]["duration_ms"] = json!(6_000);
    let err = validate_bakeoff_manifest_text_for_measurement(
        &serde_json::to_string(&post_measurement_edit).unwrap(),
        &first.manifest_sha256,
    )
    .unwrap_err();
    assert!(err.contains("post-measurement edit"), "{err}");

    let mut removed_lock = measured_manifest.clone();
    removed_lock["measurement_locks"] = json!([]);
    let err = validate_bakeoff_manifest_text_for_measurement(
        &serde_json::to_string(&removed_lock).unwrap(),
        &first.manifest_sha256,
    )
    .unwrap_err();
    assert!(err.contains("requires a measurement lock"), "{err}");

    let mut replaced_lock = measured_manifest;
    replaced_lock["measurement_locks"][0]["manifest_sha256"] = json!(hash(999_999));
    let err = validate_bakeoff_manifest_text_for_measurement(
        &serde_json::to_string(&replaced_lock).unwrap(),
        &first.manifest_sha256,
    )
    .unwrap_err();
    assert!(
        err.contains("measurement") || err.contains("attestation"),
        "{err}"
    );
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn verifies_local_audio_and_reference_bytes_without_returning_them() {
    let root = std::env::temp_dir().join(format!("voisu-bakeoff-hashes-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let mut manifest = valid_manifest();
    let mut inputs = Vec::new();
    for clip in manifest["clips"].as_array_mut().unwrap() {
        let id = clip["id"].as_str().unwrap().to_owned();
        let audio = format!("audio:{id}").into_bytes();
        let audio_path = root.join(format!("{id}.audio"));
        fs::write(&audio_path, &audio).unwrap();
        clip["audio_sha256"] = json!(digest(&audio));
        let reference_path = if clip["kind"] == "speech" {
            let reference = format!("reference:{id}").into_bytes();
            let path = root.join(format!("{id}.reference"));
            fs::write(&path, &reference).unwrap();
            clip["reference_sha256"] = json!(digest(&reference));
            Some(path)
        } else {
            None
        };
        inputs.push(BakeoffHashInput {
            clip_id: id,
            audio_path,
            reference_path,
        });
    }
    let text = serde_json::to_string(&manifest).unwrap();
    let validated = verify_bakeoff_hash_inputs(&text, &inputs).expect("matching bytes");
    assert!(validated.public_summary.get("clips").is_none());

    fs::write(&inputs[0].audio_path, b"changed").unwrap();
    let err = verify_bakeoff_hash_inputs(&text, &inputs).unwrap_err();
    assert!(err.contains("audio hash"), "{err}");
    let original_audio = format!("audio:{}", inputs[0].clip_id);
    fs::write(&inputs[0].audio_path, original_audio.as_bytes()).unwrap();
    let reference_path = inputs[1].reference_path.as_ref().expect("speech reference");
    fs::write(reference_path, b"changed reference").unwrap();
    let err = verify_bakeoff_hash_inputs(&text, &inputs).unwrap_err();
    assert!(err.contains("reference hash"), "{err}");
    let _ = fs::remove_dir_all(root);
}
