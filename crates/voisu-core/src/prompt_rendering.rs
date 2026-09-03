//! Developer Prompt Rendering domain types (DPR-T0 / #155).
//!
//! Pure vocabulary for policies, intent routes, cloud request states, timing
//! certainty, closed Structured labels, and Delivery constants. No network, no
//! daemon wiring, and no Smart Writing path changes. Later tickets (baseline,
//! router, compose) import from this module.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Fresh install / missing-key default for [`RenderingPolicy`].
///
/// Unreadable files and unknown values fail closed to [`RenderingPolicy::Natural`]
/// instead — see the load resolution in `voisu-app` config.
pub const DEFAULT_RENDERING_POLICY: RenderingPolicy = RenderingPolicy::Adaptive;

/// Hard budget from `utterance_end` to the start of Delivery handoff (ms).
pub const DELIVERY_DEADLINE_MS: u64 = 1500;

/// [`DELIVERY_DEADLINE_MS`] as a [`Duration`].
pub const DELIVERY_DEADLINE: Duration = Duration::from_millis(DELIVERY_DEADLINE_MS);

/// Delivery state for a DPR Final Transcript handoff: always unsent.
pub const DELIVERY_STATE_UNSENT: &str = "unsent";

/// DPR never auto-sends delivered text.
pub const DELIVERY_AUTO_SEND: bool = false;

/// DPR never live-types into the focused app as a streaming stream.
pub const DELIVERY_LIVE_TYPE: bool = false;

/// DPR never replaces text already delivered when late cloud arrives.
pub const DELIVERY_REPLACE_DELIVERED: bool = false;

/// Closed Structured section labels, in canonical order.
///
/// Only these labels may appear; the model or local path must not invent a
/// section the user did not speak toward.
pub const CLOSED_STRUCTURED_LABELS: &[&str] = &[
    "Goal",
    "Context",
    "Requirements",
    "Constraints",
    "Steps",
    "Acceptance Criteria",
    "Files",
    "Notes",
];

/// How aggressively Voisu organizes layout and labels for a Final Transcript.
///
/// The type is `Copy` so a Recording can snapshot the resolved policy before
/// work begins and keep that snapshot stable through Delivery — mid-utterance
/// config flips must not affect in-flight work.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderingPolicy {
    /// Clean punctuation and light layout only. No cloud. No section headers.
    Natural,
    /// Local organize always; optional cloud for disputed or complex speech.
    #[default]
    Adaptive,
    /// Prefer closed section labels when speech supports structure; cloud is
    /// required to attempt on complex structured speech (local baseline still
    /// delivers if cloud is late or rejected).
    Structured,
}

impl RenderingPolicy {
    /// The hand-authored TOML / CLI value for this policy.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Natural => "natural",
            Self::Adaptive => "adaptive",
            Self::Structured => "structured",
        }
    }

    /// Parses a CLI or config string (`natural` / `adaptive` / `structured`).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "natural" => Some(Self::Natural),
            "adaptive" => Some(Self::Adaptive),
            "structured" => Some(Self::Structured),
            _ => None,
        }
    }
}

/// Intent-routing path weight for one utterance (#141).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderingRoute {
    /// Identity / near-identity local path; no organize beyond source text.
    LiteralIdentity,
    /// Deterministic on-device organize; cloud not allowed.
    DeterministicLocal,
    /// Local baseline always; optional structured cloud call may run.
    LocalWithOptionalCloud,
}

impl RenderingRoute {
    /// Research / diagnostics wire value for this route.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LiteralIdentity => "literal_identity",
            Self::DeterministicLocal => "deterministic_local",
            Self::LocalWithOptionalCloud => "local_with_optional_cloud",
        }
    }

    /// Parses a research wire string into a route.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "literal_identity" => Some(Self::LiteralIdentity),
            "deterministic_local" => Some(Self::DeterministicLocal),
            "local_with_optional_cloud" => Some(Self::LocalWithOptionalCloud),
            _ => None,
        }
    }
}

/// Whether a structured cloud call may or must be attempted for an utterance.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudRequest {
    /// Cloud must not run (Natural policy, or route forbids it).
    NotAllowed,
    /// Cloud may run when the router elects and the deadline allows.
    Allowed,
    /// Cloud is required to attempt; late/reject still falls back to local baseline.
    Required,
}

impl CloudRequest {
    /// Research / diagnostics wire value for this state.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotAllowed => "not_allowed",
            Self::Allowed => "allowed",
            Self::Required => "required",
        }
    }

    /// Parses a research wire string into a cloud-request state.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "not_allowed" => Some(Self::NotAllowed),
            "allowed" => Some(Self::Allowed),
            "required" => Some(Self::Required),
            _ => None,
        }
    }
}

/// Certainty of optional pause-boundary timing shared by local baseline (T1)
/// and intent routing (T2). Clear evidence may drive layout; Uncertain fails closed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimingCertainty {
    /// Local rules may act on proven pause boundaries.
    Clear,
    /// Fail closed: no layout/route weight from pause timing.
    Uncertain,
}

impl TimingCertainty {
    /// Research / diagnostics wire value for this certainty.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Uncertain => "uncertain",
        }
    }

    /// Parses a research wire string into a timing certainty.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "clear" => Some(Self::Clear),
            "uncertain" => Some(Self::Uncertain),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_adaptive() {
        assert_eq!(DEFAULT_RENDERING_POLICY, RenderingPolicy::Adaptive);
        assert_eq!(RenderingPolicy::default(), RenderingPolicy::Adaptive);
    }

    #[test]
    fn policy_as_str_and_parse_round_trip() {
        for policy in [
            RenderingPolicy::Natural,
            RenderingPolicy::Adaptive,
            RenderingPolicy::Structured,
        ] {
            assert_eq!(RenderingPolicy::parse(policy.as_str()), Some(policy));
        }
        assert_eq!(RenderingPolicy::parse("future"), None);
        assert_eq!(RenderingPolicy::parse(""), None);
    }

    #[test]
    fn route_wire_names_match_research() {
        assert_eq!(RenderingRoute::LiteralIdentity.as_str(), "literal_identity");
        assert_eq!(
            RenderingRoute::DeterministicLocal.as_str(),
            "deterministic_local"
        );
        assert_eq!(
            RenderingRoute::LocalWithOptionalCloud.as_str(),
            "local_with_optional_cloud"
        );
        assert_eq!(
            RenderingRoute::parse("local_with_optional_cloud"),
            Some(RenderingRoute::LocalWithOptionalCloud)
        );
        assert_eq!(RenderingRoute::parse("other"), None);
    }

    #[test]
    fn cloud_request_wire_names_match_research() {
        assert_eq!(CloudRequest::NotAllowed.as_str(), "not_allowed");
        assert_eq!(CloudRequest::Allowed.as_str(), "allowed");
        assert_eq!(CloudRequest::Required.as_str(), "required");
        assert_eq!(
            CloudRequest::parse("required"),
            Some(CloudRequest::Required)
        );
        assert_eq!(CloudRequest::parse("maybe"), None);
    }

    #[test]
    fn closed_structured_labels_are_exactly_the_spec_list() {
        assert_eq!(
            CLOSED_STRUCTURED_LABELS,
            &[
                "Goal",
                "Context",
                "Requirements",
                "Constraints",
                "Steps",
                "Acceptance Criteria",
                "Files",
                "Notes",
            ]
        );
    }

    #[test]
    fn delivery_constants_match_spec() {
        assert_eq!(DELIVERY_DEADLINE_MS, 1500);
        assert_eq!(DELIVERY_DEADLINE, Duration::from_millis(1500));
        assert_eq!(DELIVERY_STATE_UNSENT, "unsent");
        // Bool constants are compile-time; const assert satisfies clippy::assertions_on_constants.
        const {
            assert!(!DELIVERY_AUTO_SEND);
            assert!(!DELIVERY_LIVE_TYPE);
            assert!(!DELIVERY_REPLACE_DELIVERED);
        }
    }

    #[test]
    fn rendering_policy_is_copy() {
        // Domain type is Copy so Recording can hold a snapshot without shared
        // ownership. File-backed snapshot stability is tested in voisu-app config.
        let policy = RenderingPolicy::Natural;
        let snapshot = policy;
        let held = snapshot;
        assert_eq!(held, RenderingPolicy::Natural);
        assert_eq!(policy, RenderingPolicy::Natural);
    }

    #[test]
    fn timing_certainty_wire_names_round_trip() {
        assert_eq!(TimingCertainty::Clear.as_str(), "clear");
        assert_eq!(TimingCertainty::Uncertain.as_str(), "uncertain");
        assert_eq!(
            TimingCertainty::parse("clear"),
            Some(TimingCertainty::Clear)
        );
        assert_eq!(
            TimingCertainty::parse("uncertain"),
            Some(TimingCertainty::Uncertain)
        );
        assert_eq!(TimingCertainty::parse("maybe"), None);
    }
}
