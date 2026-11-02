//! Key setup: decide, per provider, whether a pasted key is new, kept or
//! missing; check new keys against the provider; and save only when every
//! key passes. The provider round trip is injected so the decision logic is
//! tested without a network.

use std::future::Future;

use serde::Serialize;
use voisu_core::{Credential, Provider, ProviderKeyStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyState {
    Valid,
    Invalid,
    Kept,
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct KeyResult {
    pub state: KeyState,
    pub message: String,
}

impl KeyResult {
    fn new(state: KeyState, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
        }
    }

    fn passes(&self) -> bool {
        matches!(self.state, KeyState::Valid | KeyState::Kept)
    }
}

pub fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Groq => "Groq",
        Provider::Deepgram => "Deepgram",
    }
}

/// The user-facing reason for a provider's verdict. The Linux headline
/// points at `voisu setup`, which does not exist here.
pub fn status_result(provider: Provider, status: ProviderKeyStatus) -> KeyResult {
    let name = provider_name(provider);
    match status {
        ProviderKeyStatus::Valid => {
            KeyResult::new(KeyState::Valid, format!("{name} accepted this key."))
        }
        ProviderKeyStatus::InvalidKey => KeyResult::new(
            KeyState::Invalid,
            format!("{name} rejected this key. Check that you copied all of it."),
        ),
        ProviderKeyStatus::RateLimited => KeyResult::new(
            KeyState::Invalid,
            format!("{name} is rate-limiting this key right now. Try again in a minute."),
        ),
        ProviderKeyStatus::QuotaExhausted => KeyResult::new(
            KeyState::Invalid,
            format!("{name} says this key's quota is used up."),
        ),
        ProviderKeyStatus::Unreachable => KeyResult::new(
            KeyState::Invalid,
            format!("Couldn't reach {name}. Check your internet connection and try again."),
        ),
    }
}

enum Input {
    New(Credential),
    Settled(KeyResult),
}

fn classify(provider: Provider, pasted: Option<String>, saved: bool) -> Input {
    let name = provider_name(provider);
    let trimmed = pasted.as_deref().map(str::trim).unwrap_or_default();
    if trimmed.is_empty() {
        return Input::Settled(if saved {
            KeyResult::new(KeyState::Kept, "Keeping your saved key.")
        } else {
            KeyResult::new(KeyState::Missing, format!("Paste a {name} key."))
        });
    }
    match Credential::new(trimmed.to_owned()) {
        Ok(credential) => Input::New(credential),
        Err(_) => Input::Settled(KeyResult::new(
            KeyState::Invalid,
            "That doesn't look like a key.",
        )),
    }
}

/// What the user pasted (`None` or blank means "leave as is") and whether a
/// key is already stored, per provider.
pub struct KeyInputs {
    pub groq: Option<String>,
    pub deepgram: Option<String>,
    pub groq_saved: bool,
    pub deepgram_saved: bool,
}

pub struct Evaluation {
    pub groq: KeyResult,
    pub deepgram: KeyResult,
    /// Keys to store. Empty unless every provider passed and at least one
    /// key is new: a partial save would leave a half-working setup.
    pub to_store: Vec<(Provider, Credential)>,
}

pub async fn evaluate<C, F>(inputs: KeyInputs, check: C) -> Evaluation
where
    C: Fn(Provider, Credential) -> F,
    F: Future<Output = ProviderKeyStatus>,
{
    let groq = classify(Provider::Groq, inputs.groq, inputs.groq_saved);
    let deepgram = classify(Provider::Deepgram, inputs.deepgram, inputs.deepgram_saved);

    let run = |provider: Provider, input: Input| {
        let check = &check;
        async move {
            match input {
                Input::New(credential) => {
                    let status = check(provider, credential.clone()).await;
                    (status_result(provider, status), Some(credential))
                }
                Input::Settled(result) => (result, None),
            }
        }
    };
    let ((groq, groq_new), (deepgram, deepgram_new)) =
        tokio::join!(run(Provider::Groq, groq), run(Provider::Deepgram, deepgram));

    let to_store = if groq.passes() && deepgram.passes() {
        [
            (Provider::Groq, groq_new),
            (Provider::Deepgram, deepgram_new),
        ]
        .into_iter()
        .filter_map(|(provider, credential)| credential.map(|c| (provider, c)))
        .collect()
    } else {
        Vec::new()
    };
    Evaluation {
        groq,
        deepgram,
        to_store,
    }
}

/// Whether the setup window opens at launch: always on a manual launch, and
/// on a login launch only when keys are missing.
pub fn should_open_window(launched_at_login: bool, keys_ready: bool) -> bool {
    !launched_at_login || !keys_ready
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn inputs(groq: Option<&str>, deepgram: Option<&str>, saved: (bool, bool)) -> KeyInputs {
        KeyInputs {
            groq: groq.map(str::to_owned),
            deepgram: deepgram.map(str::to_owned),
            groq_saved: saved.0,
            deepgram_saved: saved.1,
        }
    }

    /// A checker that accepts only `good-*` keys and records what it saw.
    fn checker(
        seen: &Mutex<Vec<(Provider, String)>>,
    ) -> impl Fn(Provider, Credential) -> std::future::Ready<ProviderKeyStatus> + '_ {
        move |provider, credential| {
            let key = credential.expose_to_boundary().to_owned();
            let status = if key.starts_with("good-") {
                ProviderKeyStatus::Valid
            } else {
                ProviderKeyStatus::InvalidKey
            };
            seen.lock().unwrap().push((provider, key));
            std::future::ready(status)
        }
    }

    fn stored(evaluation: &Evaluation) -> Vec<(Provider, String)> {
        evaluation
            .to_store
            .iter()
            .map(|(p, c)| (*p, c.expose_to_boundary().to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn first_setup_saves_both_trimmed_keys_when_both_pass() {
        let seen = Mutex::new(Vec::new());
        let evaluation = evaluate(
            inputs(Some("  good-g \n"), Some("good-d"), (false, false)),
            checker(&seen),
        )
        .await;
        assert_eq!(evaluation.groq.state, KeyState::Valid);
        assert_eq!(evaluation.deepgram.state, KeyState::Valid);
        assert_eq!(
            stored(&evaluation),
            vec![
                (Provider::Groq, "good-g".to_owned()),
                (Provider::Deepgram, "good-d".to_owned())
            ]
        );
    }

    #[tokio::test]
    async fn one_rejected_key_saves_nothing() {
        let seen = Mutex::new(Vec::new());
        let evaluation = evaluate(
            inputs(Some("good-g"), Some("bad-d"), (false, false)),
            checker(&seen),
        )
        .await;
        assert_eq!(evaluation.groq.state, KeyState::Valid);
        assert_eq!(evaluation.deepgram.state, KeyState::Invalid);
        assert!(evaluation.deepgram.message.contains("Deepgram rejected"));
        assert!(evaluation.to_store.is_empty());
    }

    #[tokio::test]
    async fn a_missing_key_on_first_setup_saves_nothing_but_the_pasted_one_is_checked() {
        let seen = Mutex::new(Vec::new());
        let evaluation =
            evaluate(inputs(Some("good-g"), None, (false, false)), checker(&seen)).await;
        assert_eq!(evaluation.deepgram.state, KeyState::Missing);
        assert!(evaluation.to_store.is_empty());
        // The pasted Groq key is still checked so its result is shown.
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn blank_field_keeps_a_saved_key_and_replaces_only_the_new_one() {
        let seen = Mutex::new(Vec::new());
        let evaluation = evaluate(
            inputs(Some("good-g2"), Some("   "), (true, true)),
            checker(&seen),
        )
        .await;
        assert_eq!(evaluation.deepgram.state, KeyState::Kept);
        assert_eq!(
            stored(&evaluation),
            vec![(Provider::Groq, "good-g2".to_owned())]
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "a kept key is not re-checked"
        );
    }

    #[tokio::test]
    async fn nothing_new_stores_nothing() {
        let seen = Mutex::new(Vec::new());
        let evaluation = evaluate(inputs(None, None, (true, true)), checker(&seen)).await;
        assert!(evaluation.to_store.is_empty());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_key_with_an_inner_line_break_is_rejected_without_a_request() {
        let seen = Mutex::new(Vec::new());
        let evaluation = evaluate(
            inputs(Some("good-g\nextra"), Some("good-d"), (false, false)),
            checker(&seen),
        )
        .await;
        assert_eq!(evaluation.groq.state, KeyState::Invalid);
        assert!(evaluation.to_store.is_empty());
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn every_failing_status_is_invalid_and_names_the_provider() {
        for status in [
            ProviderKeyStatus::InvalidKey,
            ProviderKeyStatus::RateLimited,
            ProviderKeyStatus::QuotaExhausted,
            ProviderKeyStatus::Unreachable,
        ] {
            let result = status_result(Provider::Groq, status);
            assert_eq!(result.state, KeyState::Invalid);
            assert!(result.message.contains("Groq"), "{}", result.message);
            assert!(!result.message.contains("voisu setup"));
        }
    }

    #[test]
    fn window_opens_on_manual_launch_or_when_keys_are_missing() {
        assert!(should_open_window(false, true));
        assert!(should_open_window(false, false));
        assert!(should_open_window(true, false));
        assert!(!should_open_window(true, true));
    }

    #[test]
    fn key_state_serializes_for_the_ui_contract() {
        let json = serde_json::to_string(&KeyResult::new(KeyState::Kept, "x")).unwrap();
        assert_eq!(json, r#"{"state":"kept","message":"x"}"#);
    }
}
