//! Classification of a live per-provider credential round trip. The mapping is
//! pure, so every surface (setup wizard, `voisu doctor`, `voisu auth verify`,
//! daemon logs) reports the same actionable meaning without a network.

use voisu_core::{Provider, ProviderKeyStatus, ReadinessStatus, provider_free_tier_hint};

#[test]
fn a_success_status_is_valid() {
    assert_eq!(ProviderKeyStatus::classify(200, false), ProviderKeyStatus::Valid);
    assert_eq!(ProviderKeyStatus::classify(204, false), ProviderKeyStatus::Valid);
    assert!(ProviderKeyStatus::Valid.is_valid());
}

#[test]
fn unauthorized_and_forbidden_mean_the_key_is_invalid() {
    assert_eq!(ProviderKeyStatus::classify(401, false), ProviderKeyStatus::InvalidKey);
    assert_eq!(ProviderKeyStatus::classify(403, false), ProviderKeyStatus::InvalidKey);
    // The remediation is named in the headline so every surface tells the user
    // exactly what to do.
    assert!(ProviderKeyStatus::InvalidKey.headline().contains("voisu setup"));
    assert_eq!(ProviderKeyStatus::InvalidKey.readiness(), ReadinessStatus::Fail);
    assert!(!ProviderKeyStatus::InvalidKey.is_transient());
}

#[test]
fn a_429_with_retry_after_is_a_transient_rate_limit() {
    assert_eq!(
        ProviderKeyStatus::classify(429, true),
        ProviderKeyStatus::RateLimited
    );
    assert!(ProviderKeyStatus::RateLimited.is_transient());
    assert_eq!(ProviderKeyStatus::RateLimited.readiness(), ReadinessStatus::Warn);
}

#[test]
fn a_bare_429_is_quota_exhaustion() {
    assert_eq!(
        ProviderKeyStatus::classify(429, false),
        ProviderKeyStatus::QuotaExhausted
    );
    assert!(ProviderKeyStatus::QuotaExhausted.is_transient());
    assert_eq!(
        ProviderKeyStatus::QuotaExhausted.readiness(),
        ReadinessStatus::Warn
    );
}

#[test]
fn other_statuses_are_transient_unreachable() {
    for status in [408_u16, 500, 502, 503, 0] {
        assert_eq!(
            ProviderKeyStatus::classify(status, false),
            ProviderKeyStatus::Unreachable,
            "status {status} should be Unreachable"
        );
    }
    assert!(ProviderKeyStatus::Unreachable.is_transient());
    assert_eq!(ProviderKeyStatus::Unreachable.readiness(), ReadinessStatus::Warn);
}

#[test]
fn free_tier_hints_name_the_provider_console() {
    assert!(provider_free_tier_hint(Provider::Deepgram).contains("console.deepgram.com"));
    assert!(provider_free_tier_hint(Provider::Groq).contains("console.groq.com"));
}
