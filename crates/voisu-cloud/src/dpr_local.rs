//! The portable, network-free half of Developer Prompt Rendering: Source
//! Transcript selection, protected-token extraction, and the local formatting
//! render. The Linux daemon (voisu-app `dpr_pipeline`) and the Windows app
//! share it, so local formatting renders identically on both.

use voisu_core::{
    ComposeSource, IntentObservation, LocalBaseline, LocalBaselineOptions,
    MAX_COMPOSE_FIELD_UTF8_BYTES, Provider, ProviderState, RenderingPolicy, RoutingDecision,
    SourceSelection, SourceTranscript, SttProvider, TranscriptSelection, organize_local_baseline,
    route_intent, sanitize_source_transcripts,
};

use crate::dpr_cloud::MAX_DPR_PROTECTED_TOKENS;

/// Routes one utterance and organizes its deterministic local baseline: the
/// text Developer Prompt Rendering delivers whenever no cloud candidate is
/// accepted.
#[must_use]
pub fn route_and_organize_local(
    observation: &IntentObservation,
) -> (RoutingDecision, LocalBaseline) {
    let routing = route_intent(observation);
    let baseline = organize_local_baseline(
        &observation.primary_text,
        &LocalBaselineOptions {
            policy: observation.policy,
            route: routing.route,
            timing: None,
        },
    );
    (routing, baseline)
}

/// Local formatting for a validated Transcript: exactly what the Linux daemon
/// delivers with Developer Prompt Rendering on and no cloud formatter. Spoken
/// "new line" / "new paragraph" become line breaks; spoken marks, quotes, and
/// first/second lists are organized. `None` when no Source Transcript carries
/// text. Callers gate this on English eligibility, as Linux does.
#[must_use]
pub fn render_dpr_local(
    validated_transcript: &str,
    sources: &[SourceTranscript],
    dictionary_terms: &[String],
    policy: RenderingPolicy,
) -> Option<String> {
    let context = dpr_source_context(sources, dictionary_terms)?;
    let (_, baseline) = route_and_organize_local(&IntentObservation {
        policy,
        primary_text: validated_transcript.to_owned(),
        provider_state: context.provider_state,
        surface_hint: None,
        process_hint: None,
        timing: None,
    });
    Some(baseline.rendered().to_owned())
}

/// Owned adapter from the daemon's Source Transcripts into the generic
/// provider-a/provider-b compose contract. Selection is deliberately local:
/// DPR must not spend a legacy free-form reconciliation call before its one
/// permitted structured cloud attempt.
pub struct DprSourceContext {
    pub selected_source: String,
    pub sources: Vec<ComposeSource>,
    pub source_selection: SourceSelection,
    pub provider_state: ProviderState,
    pub transcript_selection: TranscriptSelection,
}

#[must_use]
pub fn dpr_source_context(
    sources: &[SourceTranscript],
    dictionary_terms: &[String],
) -> Option<DprSourceContext> {
    // Quality-classify before selection so pure-outro Groq never wins over empty
    // Deepgram and a trailing anchored outro never enters compose/Delivery.
    let sanitized = sanitize_source_transcripts(sources.iter().cloned());
    let available: Vec<&SourceTranscript> = sanitized
        .iter()
        .filter(|source| !source.text.is_empty())
        .collect();
    let selected = available
        .iter()
        .find(|source| source.provider == Provider::Groq)
        .copied()
        .or_else(|| available.first().copied())?;
    let selected_provider = selected.provider;
    let exact_agreement = available.len() > 1
        && available
            .iter()
            .all(|source| source.text == available[0].text);
    let punctuation_only_agreement = available.len() > 1
        && !exact_agreement
        && available.iter().all(|source| {
            normalized_source_words(&source.text) == normalized_source_words(&available[0].text)
        });
    let other = available
        .iter()
        .find(|source| source.provider != selected_provider)
        .copied();
    let safe_complementary =
        if available.len() == 2 && !exact_agreement && !punctuation_only_agreement {
            other.and_then(|other| merge_insertion_only_sources(&selected.text, &other.text))
        } else {
            None
        };
    let protected_disagreement = safe_complementary.is_none()
        && other.is_some_and(|other| {
            protected_atoms_disagree(&selected.text, &other.text, dictionary_terms)
        });
    let provider_state = if available.len() <= 1 {
        ProviderState::SingleProvider
    } else if exact_agreement {
        ProviderState::ExactAgreement
    } else if punctuation_only_agreement {
        ProviderState::PunctuationOnlyAgreement
    } else if safe_complementary.is_some() {
        ProviderState::SafeComplementary
    } else if protected_disagreement {
        ProviderState::ProtectedTokenDisagreement
    } else {
        ProviderState::SemanticDisagreement
    };
    let reason = if available.len() <= 1 {
        "only_available"
    } else if exact_agreement {
        "exact_agreement"
    } else if punctuation_only_agreement {
        "punctuation_local_render"
    } else if safe_complementary.is_some() {
        "safe_complementary_merge"
    } else {
        "configured_primary_rank"
    };

    let mut compose_sources = Vec::with_capacity(available.len().min(2));
    compose_sources.push(ComposeSource {
        provider: SttProvider::ProviderA,
        available: true,
        text: selected.text.clone(),
        primary: true,
    });
    if let Some(other) = other {
        compose_sources.push(ComposeSource {
            provider: SttProvider::ProviderB,
            available: true,
            text: other.text.clone(),
            primary: false,
        });
    }

    Some(DprSourceContext {
        selected_source: safe_complementary.unwrap_or_else(|| selected.text.clone()),
        sources: compose_sources,
        source_selection: SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: reason.to_owned(),
        },
        provider_state,
        transcript_selection: if provider_state == ProviderState::SafeComplementary {
            TranscriptSelection::Complementary
        } else {
            match selected_provider {
                Provider::Groq if punctuation_only_agreement => {
                    TranscriptSelection::NearIdenticalGroq
                }
                Provider::Groq => TranscriptSelection::SourceGroq,
                Provider::Deepgram => TranscriptSelection::SourceDeepgram,
            }
        },
    })
}

fn protected_atoms_disagree(left: &str, right: &str, dictionary_terms: &[String]) -> bool {
    let left_protected: Vec<String> = dpr_protected_tokens(left, dictionary_terms)
        .into_iter()
        .filter(|token| !is_closed_negation(token))
        .collect();
    let right_protected: Vec<String> = dpr_protected_tokens(right, dictionary_terms)
        .into_iter()
        .filter(|token| !is_closed_negation(token))
        .collect();
    left_protected.iter().any(|token| !right.contains(token))
        || right_protected.iter().any(|token| !left.contains(token))
}

fn is_closed_negation(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "no" | "not"
            | "never"
            | "cannot"
            | "can't"
            | "cant"
            | "don't"
            | "dont"
            | "won't"
            | "wont"
            | "isn't"
            | "isnt"
            | "aren't"
            | "arent"
            | "ain't"
            | "aint"
    )
}

/// Conservatively merges two whitespace-token streams only when their shared
/// anchors remain ordered and at most one provider contributes tokens in each
/// gap. If both providers offer different text for the same gap, the evidence
/// is disputed and no merge is produced. The token cap keeps the bounded
/// look-ahead local even for an adversarially large Source Transcript.
fn merge_insertion_only_sources(left: &str, right: &str) -> Option<String> {
    const MAX_SAFE_COMPLEMENT_TOKENS: usize = 128;
    let left_tokens: Vec<&str> = left.split_whitespace().collect();
    let right_tokens: Vec<&str> = right.split_whitespace().collect();
    if left_tokens.is_empty()
        || right_tokens.is_empty()
        || left_tokens.len() > MAX_SAFE_COMPLEMENT_TOKENS
        || right_tokens.len() > MAX_SAFE_COMPLEMENT_TOKENS
    {
        return None;
    }

    let mut merged = Vec::with_capacity(left_tokens.len().saturating_add(right_tokens.len()));
    let mut left_at = 0usize;
    let mut right_at = 0usize;
    let mut shared_anchors = 0usize;
    let mut left_contributed = false;
    let mut right_contributed = false;

    while left_at < left_tokens.len() || right_at < right_tokens.len() {
        let mut next_anchor = None;
        let mut nearest_distance = usize::MAX;
        for (left_index, left_token) in left_tokens.iter().enumerate().skip(left_at) {
            for (right_index, right_token) in right_tokens.iter().enumerate().skip(right_at) {
                if token_anchor_eq(left_token, right_token) {
                    let distance =
                        left_index.saturating_sub(left_at) + right_index.saturating_sub(right_at);
                    if distance < nearest_distance {
                        nearest_distance = distance;
                        next_anchor = Some((left_index, right_index));
                    }
                }
            }
        }

        let Some((left_anchor, right_anchor)) = next_anchor else {
            let left_gap = &left_tokens[left_at..];
            let right_gap = &right_tokens[right_at..];
            if !left_gap.is_empty() && !right_gap.is_empty() {
                return None;
            }
            if !left_gap
                .iter()
                .chain(right_gap)
                .all(|token| safe_complement_token(token))
            {
                return None;
            }
            left_contributed |= !left_gap.is_empty();
            right_contributed |= !right_gap.is_empty();
            merged.extend_from_slice(left_gap);
            merged.extend_from_slice(right_gap);
            break;
        };

        let left_gap = &left_tokens[left_at..left_anchor];
        let right_gap = &right_tokens[right_at..right_anchor];
        if !left_gap.is_empty() && !right_gap.is_empty() {
            return None;
        }
        if !left_gap
            .iter()
            .chain(right_gap)
            .all(|token| safe_complement_token(token))
        {
            return None;
        }
        left_contributed |= !left_gap.is_empty();
        right_contributed |= !right_gap.is_empty();
        merged.extend_from_slice(left_gap);
        merged.extend_from_slice(right_gap);
        merged.push(left_tokens[left_anchor]);
        shared_anchors += 1;
        left_at = left_anchor + 1;
        right_at = right_anchor + 1;
    }

    (shared_anchors >= 2 && left_contributed && right_contributed).then(|| merged.join(" "))
}

fn safe_complement_token(token: &str) -> bool {
    token.bytes().any(|byte| byte.is_ascii_digit())
        || token.contains("//")
        || token.contains('/')
        || token.contains('\\')
        || token.starts_with('-')
        || token.contains('_')
        || token.contains("::")
        || token.contains(['(', ')', '[', ']', '{', '}', '=', '@'])
}

fn token_anchor_eq(left: &str, right: &str) -> bool {
    let normalize = |token: &str| {
        token
            .trim_matches(|character: char| !character.is_alphanumeric())
            .to_ascii_lowercase()
    };
    let left = normalize(left);
    !left.is_empty() && left == normalize(right)
}

fn normalized_source_words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric() && character != '\'')
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Extract closed, host-verifiable protected atoms from the selected source.
/// The model never decides this list. Ambiguous natural-language entities stay
/// untouched by this conservative recognizer; exact dictionary terms and
/// mechanically recognizable technical/semantic atoms fail closed.
#[must_use]
pub fn dpr_protected_tokens(selected_source: &str, dictionary_terms: &[String]) -> Vec<String> {
    let mut protected = Vec::new();
    for raw in selected_source.split_whitespace() {
        let token = raw.trim_matches(|character: char| {
            matches!(character, ',' | ';' | '!' | '?' | '"' | '\'' | '`')
        });
        let lower = token.to_ascii_lowercase();
        let negation = matches!(
            lower.as_str(),
            "no" | "not"
                | "never"
                | "cannot"
                | "can't"
                | "cant"
                | "don't"
                | "dont"
                | "won't"
                | "wont"
                | "isn't"
                | "isnt"
                | "aren't"
                | "arent"
                | "ain't"
                | "aint"
        );
        let technical = token.bytes().any(|byte| byte.is_ascii_digit())
            || token.contains("://")
            || token.contains('/')
            || token.contains('\\')
            || token.starts_with('-')
            || token.contains('_')
            || token.contains("::")
            || token.contains(['(', ')', '[', ']', '{', '}', '=', '@'])
            || is_dot_technical_token(token);
        if negation || technical {
            push_protected(&mut protected, selected_source, token);
        }
    }

    let lower = selected_source.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(open_relative) = lower[search_from..].find("quote ") {
        let interior_start = search_from + open_relative + "quote ".len();
        let Some(close_relative) = lower[interior_start..].find(" unquote") else {
            break;
        };
        let interior_end = interior_start + close_relative;
        push_protected(
            &mut protected,
            selected_source,
            &selected_source[interior_start..interior_end],
        );
        search_from = interior_end + " unquote".len();
    }

    // Organized `"…"` spans (including interiors from `quote,` … `unquote`).
    let mut quoted_from = 0;
    while let Some(open_relative) = selected_source[quoted_from..].find('"') {
        let interior_start = quoted_from + open_relative + 1;
        let Some(close_relative) = selected_source[interior_start..].find('"') else {
            break;
        };
        push_protected(
            &mut protected,
            selected_source,
            &selected_source[interior_start..interior_start + close_relative],
        );
        quoted_from = interior_start + close_relative + 1;
    }

    // Dictionary entries are important spelling/name evidence, but cannot
    // crowd closed technical or semantic atoms out of the bounded request.
    for term in dictionary_terms {
        push_protected(&mut protected, selected_source, term);
    }

    protected
}

/// `cargo.toml` / `.env` are technical. A sentence-final `hello.` is not.
fn is_dot_technical_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    if bytes.first() == Some(&b'.')
        && bytes
            .get(1)
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
    {
        return true;
    }
    bytes.windows(3).any(|window| {
        window[1] == b'.' && window[0].is_ascii_alphanumeric() && window[2].is_ascii_alphanumeric()
    })
}

fn push_protected(protected: &mut Vec<String>, source: &str, token: &str) {
    if protected.len() >= MAX_DPR_PROTECTED_TOKENS
        || token.is_empty()
        || token.len() > MAX_COMPOSE_FIELD_UTF8_BYTES
        || !source.contains(token)
        || protected.iter().any(|existing| existing == token)
    {
        return;
    }
    protected.push(token.to_owned());
}

#[cfg(test)]
mod tests {
    use voisu_core::DEFAULT_RENDERING_POLICY;

    use super::*;

    fn sources(groq: &str, deepgram: &str) -> Vec<SourceTranscript> {
        vec![
            SourceTranscript {
                provider: Provider::Groq,
                text: groq.to_owned(),
            },
            SourceTranscript {
                provider: Provider::Deepgram,
                text: deepgram.to_owned(),
            },
        ]
    }

    fn render(validated: &str) -> Option<String> {
        render_dpr_local(
            validated,
            &sources(validated, validated),
            &[],
            DEFAULT_RENDERING_POLICY,
        )
    }

    #[test]
    fn spoken_line_and_paragraph_commands_become_real_breaks() {
        assert_eq!(
            render("first thought new line second thought").as_deref(),
            Some("First thought.\nSecond thought")
        );
        assert_eq!(
            render("hello new paragraph world").as_deref(),
            Some("Hello.\n\nWorld.")
        );
    }

    #[test]
    fn no_source_with_text_renders_nothing() {
        assert_eq!(
            render_dpr_local(
                "first thought new line second thought",
                &sources("", ""),
                &[],
                DEFAULT_RENDERING_POLICY,
            ),
            None
        );
    }
}
