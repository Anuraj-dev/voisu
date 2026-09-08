//! L0-DEFECT (plan R4): Groq's endpoint gate is not a catalog parser.
//!
//! Current behavior: [`voisu_app::system::provider_endpoint_url`] accepts any
//! well-formed HTTPS URL without userinfo, plus HTTP on loopback. That is a
//! live-provider transport policy.
//!
//! Required future contract (L3 downloader, not implemented here): parse
//! structurally — HTTPS only, no userinfo, at most three redirects, every
//! redirect restricted to explicitly cataloged HTTPS hosts. Do not reuse this
//! Groq gate. No free-form download URL.

use voisu_app::system::provider_endpoint_url;

#[test]
fn groq_endpoint_gate_accepts_uncataloged_https_hosts() {
    // Current: any HTTPS host without userinfo is allowed.
    assert!(
        provider_endpoint_url("https://evil.example/ggml-model.bin").is_some(),
        "current Groq gate is not a catalog: uncataloged HTTPS must still parse"
    );
    assert!(provider_endpoint_url("https://attacker.test/weights.bin").is_some());
    assert!(provider_endpoint_url("https://api.groq.com/openai/v1/audio/transcriptions").is_some());
}

#[test]
fn groq_endpoint_gate_allows_http_loopback_which_r4_must_not() {
    // Current transport exception for local test servers.
    assert!(provider_endpoint_url("http://localhost:8080/transcribe").is_some());
    assert!(provider_endpoint_url("http://127.0.0.1:9999/transcribe").is_some());
    // R4 required: HTTPS only — even loopback HTTP is not a catalog download.
    // Named gap: this function still returns Some for those URLs.
}

#[test]
fn groq_endpoint_gate_still_rejects_userinfo_and_plain_http_remote() {
    // Parsing (not string-prefix matching) already holds for the transport gate.
    assert!(provider_endpoint_url("http://attacker.example/model.bin").is_none());
    assert!(provider_endpoint_url("https://user:pass@huggingface.co/model.bin").is_none());
    assert!(provider_endpoint_url("https://localhost:8080@attacker.example/model.bin").is_none());
}

#[test]
fn r4_catalog_contract_is_not_implemented_by_the_groq_gate() {
    // Required L3 contract, documented here so it cannot be mistaken for done:
    // a catalog URL helper would reject uncataloged HTTPS and HTTP loopback.
    // `provider_endpoint_url` is the opposite of that helper.
    let uncataloged_https = "https://models.example/whisper.bin";
    let loopback_http = "http://127.0.0.1:8080/whisper.bin";
    assert!(
        provider_endpoint_url(uncataloged_https).is_some()
            && provider_endpoint_url(loopback_http).is_some(),
        "L0-DEFECT: Groq transport still accepts what R4 catalog parsing must reject"
    );
}
