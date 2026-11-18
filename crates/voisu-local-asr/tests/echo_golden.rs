//! The Rust prompt-echo filter matches the Python reference verdicts.
//! Regenerate `golden/echo.json` with `golden/gen_echo.py`.

use serde_json::Value;
use voisu_local_asr::echo::is_prompt_echo;

#[test]
fn echo_verdicts_match_the_python_reference() {
    let golden: Value = serde_json::from_str(include_str!("golden/echo.json")).unwrap();
    let cases = golden["cases"].as_array().unwrap();
    assert!(cases.len() > 30);
    let mut echoes = 0;
    for case in cases {
        let prompt = case["prompt"].as_str().unwrap();
        let text = case["text"].as_str().unwrap();
        let expected = case["echo"].as_bool().unwrap();
        echoes += usize::from(expected);
        assert_eq!(
            is_prompt_echo(text, prompt),
            expected,
            "{prompt:?} / {text:?}"
        );
    }
    // Both verdicts are exercised, not just one.
    assert!(echoes >= 8 && echoes < cases.len() - 8);
}
