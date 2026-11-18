//! The Rust splitter matches the Python reference on synthetic PCM.
//! Regenerate `golden/windows.json` with `golden/gen_windows.py`.

use serde_json::Value;
use voisu_local_asr::window::{Cut, TooManyWindows, plan_windows};

fn segment(kind: &str, count: usize, amp: i64, seed: u64) -> Vec<i16> {
    match kind {
        "silence" => vec![0; count],
        "noise" => {
            let mut state = seed;
            (0..count)
                .map(|_| {
                    state = (state * 1_103_515_245 + 12_345) % 2_147_483_648;
                    let value = ((state >> 16) % (2 * amp as u64 + 1)) as i64 - amp;
                    i16::try_from(value).unwrap()
                })
                .collect()
        }
        "square" => {
            let half = seed as usize;
            let amp = i16::try_from(amp).unwrap();
            (0..count)
                .map(|i| {
                    if (i / half).is_multiple_of(2) {
                        amp
                    } else {
                        -amp
                    }
                })
                .collect()
        }
        other => panic!("unknown segment kind {other}"),
    }
}

fn build(segments: &[Value]) -> Vec<i16> {
    let mut samples = Vec::new();
    for (index, item) in segments.iter().enumerate() {
        let kind = item[0].as_str().unwrap();
        let count = usize::try_from(item[1].as_u64().unwrap()).unwrap();
        let amp = item[2].as_i64().unwrap();
        let seed = if kind == "square" {
            40
        } else {
            index as u64 + 1
        };
        samples.extend(segment(kind, count, amp, seed));
    }
    samples
}

fn cut_name(cut: Cut) -> &'static str {
    match cut {
        Cut::Quiet => "quiet",
        Cut::Hard => "hard",
        Cut::End => "end",
    }
}

#[test]
fn windows_match_the_python_reference() {
    let golden: Value = serde_json::from_str(include_str!("golden/windows.json")).unwrap();
    let cases = golden["cases"].as_array().unwrap();
    assert!(cases.len() >= 6);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let samples = build(case["segments"].as_array().unwrap());
        let planned = plan_windows(&samples);
        if let Some(error) = case.get("error") {
            assert_eq!(error, "too_many_windows", "{name}");
            assert_eq!(planned, Err(TooManyWindows), "{name}");
            continue;
        }
        let actual: Vec<Value> = planned
            .unwrap_or_else(|_| panic!("{name} should plan"))
            .into_iter()
            .map(|w| serde_json::json!([w.start, w.end, cut_name(w.cut)]))
            .collect();
        assert_eq!(&Value::Array(actual), &case["windows"], "{name}");
    }
}
