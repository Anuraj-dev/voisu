// Provider HTTP client: curl-based authenticated probes for Groq/Deepgram endpoints.
//
// Split out of system.rs as a pure move; module-global items come from `super`.

use super::*;

pub struct ProviderHttpClient;

/// A credentialed provider request with no response body retained. The next
/// provider adapter can supply its own endpoint while reusing this process and
/// environment boundary.
pub struct ProviderHttpRequest {
    pub url: &'static str,
    pub authorization_scheme: &'static str,
}

impl ProviderHttpClient {
    /// Runs the shared authenticated provider request boundary and returns its
    /// HTTP status together with whether the response carried a `Retry-After`
    /// header. Future Groq transcription can reuse this async boundary without
    /// inheriting credentials or curl configuration from the CLI.
    pub async fn authenticated_status(
        &self,
        credential: Credential,
        request: ProviderHttpRequest,
    ) -> Result<AuthProbe, BoundaryError> {
        tokio::task::spawn_blocking(move || authenticated_status(credential, request))
            .await
            .map_err(|_| {
                BoundaryError::new(
                    BoundaryKind::ProviderAuthentication,
                    "provider request task failed",
                )
            })?
    }

    /// The endpoint used for the cheapest authenticated round trip per provider.
    fn probe_request(provider: Provider) -> ProviderHttpRequest {
        match provider {
            Provider::Groq => ProviderHttpRequest {
                url: "https://api.groq.com/openai/v1/models",
                authorization_scheme: "Bearer",
            },
            Provider::Deepgram => ProviderHttpRequest {
                url: "https://api.deepgram.com/v1/projects",
                authorization_scheme: "Token",
            },
        }
    }

    /// Performs a live credential round trip and classifies the outcome. A
    /// transport failure (curl missing, timeout, connection refused) is a
    /// transient `Unreachable`, never a wrong-key verdict. Tests bypass the
    /// network via `VOISU_TEST_AUTH_{GROQ,DEEPGRAM}` (see `controlled_key_status`).
    pub async fn check(&self, provider: Provider, credential: Credential) -> ProviderKeyStatus {
        let controlled = match provider {
            Provider::Groq => std::env::var_os("VOISU_TEST_AUTH_GROQ"),
            Provider::Deepgram => std::env::var_os("VOISU_TEST_AUTH_DEEPGRAM"),
        };
        if let Some(mode) = controlled {
            return controlled_key_status(&mode.to_string_lossy());
        }
        match self
            .authenticated_status(credential, Self::probe_request(provider))
            .await
        {
            Ok(probe) => ProviderKeyStatus::classify(probe.status, probe.retry_after),
            Err(_) => ProviderKeyStatus::Unreachable,
        }
    }

    /// Verifies a credential, mapping a non-valid classification onto a
    /// `BoundaryError` whose public message is the same actionable headline
    /// every other surface shows.
    pub async fn verify(
        &self,
        provider: Provider,
        credential: Credential,
    ) -> Result<(), BoundaryError> {
        match self.check(provider, credential).await {
            ProviderKeyStatus::Valid => Ok(()),
            status => Err(BoundaryError::new(
                BoundaryKind::ProviderAuthentication,
                "provider credential round trip did not authenticate",
            )
            .with_public_message(status.headline())),
        }
    }
}

impl ProviderAuthenticator for ProviderHttpClient {
    fn verify(&mut self, provider: Provider, credential: Credential) -> BoundaryFuture<'_, ()> {
        Box::pin(async move { ProviderHttpClient.verify(provider, credential).await })
    }
}

/// The HTTP status of an authenticated probe plus whether a `Retry-After`
/// header accompanied it, which distinguishes a transient rate limit from a
/// spent quota on a bare 429.
pub struct AuthProbe {
    pub status: u16,
    pub retry_after: bool,
}

/// Maps a `VOISU_TEST_AUTH_*` seam value onto a classification so tests exercise
/// every branch without touching the network. `authorized` stays the historic
/// success token; `denied` stays the historic rejection (a wrong key).
fn controlled_key_status(mode: &str) -> ProviderKeyStatus {
    match mode {
        "authorized" | "valid" | "200" => ProviderKeyStatus::Valid,
        "denied" | "invalid" | "401" | "403" => ProviderKeyStatus::InvalidKey,
        "ratelimited" | "429-retry" => ProviderKeyStatus::RateLimited,
        "quota" | "429" => ProviderKeyStatus::QuotaExhausted,
        "unreachable" | "500" | "502" | "503" => ProviderKeyStatus::Unreachable,
        other => match other.parse::<u16>() {
            Ok(status) => ProviderKeyStatus::classify(status, false),
            Err(_) => ProviderKeyStatus::Unreachable,
        },
    }
}

fn authenticated_status(
    credential: Credential,
    request: ProviderHttpRequest,
) -> Result<AuthProbe, BoundaryError> {
    let credential = curl_config_escape(credential.expose_to_boundary());
    let config = format!(
        "url = \"{}\"\nheader = \"Authorization: {} {credential}\"\n",
        request.url, request.authorization_scheme,
    );
    // `--fail` is deliberately omitted: it makes curl exit non-zero on a 4xx/5xx
    // and swallows the status, collapsing 401/403/429/5xx into one opaque error.
    // Without it curl completes with exit 0 and writes the code, so the caller
    // can classify. A non-zero exit now means a genuine transport failure.
    let outcome = run_restricted(
        "curl",
        &[
            "-q",
            "--config",
            "-",
            "--silent",
            "--show-error",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}\t%header{retry-after}",
            "--max-time",
            "2",
        ],
        Some(config.as_bytes()),
        true,
    )
    .map_err(provider_authentication_error)?;
    if !outcome.success {
        return Err(BoundaryError::new(
            BoundaryKind::ProviderAuthentication,
            "provider request did not complete",
        ));
    }
    let rendered = std::str::from_utf8(&outcome.stdout).map_err(|_| {
        BoundaryError::new(
            BoundaryKind::ProviderAuthentication,
            "provider returned no HTTP status",
        )
    })?;
    parse_auth_probe(rendered).ok_or_else(|| {
        BoundaryError::new(
            BoundaryKind::ProviderAuthentication,
            "provider returned no HTTP status",
        )
    })
}

/// Parses curl's `%{http_code}\t%header{retry-after}` write-out. The status is
/// the first tab-separated field; `Retry-After` is present when the second
/// field is non-empty and is a real value (older curl that lacks `%header{}`
/// writes the literal token, which is treated as absent).
fn parse_auth_probe(rendered: &str) -> Option<AuthProbe> {
    let line = rendered.trim_end_matches(['\r', '\n']);
    let mut fields = line.splitn(2, '\t');
    let status = fields.next()?.trim().parse::<u16>().ok()?;
    let retry_after = fields
        .next()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty() && !value.starts_with('%'));
    Some(AuthProbe {
        status,
        retry_after,
    })
}

fn provider_authentication_error(error: ProcessError) -> BoundaryError {
    let detail = match error {
        ProcessError::Unavailable => "curl unavailable",
        ProcessError::Input => "curl rejected credential input",
        ProcessError::TimedOut => "curl deadline elapsed",
        ProcessError::Wait | ProcessError::Output => "curl execution failed",
    };
    BoundaryError::new(BoundaryKind::ProviderAuthentication, detail)
}

/// Resolve the current display session from the live environment. Detection is
/// pure logic in `voisu-core`; this only reads the environment for it.
pub(super) fn current_session() -> SessionResolution {
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let x11_display = std::env::var("DISPLAY").ok();
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    resolve_session(
        wayland_display.as_deref(),
        x11_display.as_deref(),
        session_type.as_deref(),
    )
}

impl ProviderHttpClient {
    pub(super) async fn transcribe_groq_chunk(
        &self,
        credential: Credential,
        endpoint: String,
        params: GroqRequestParams,
        pcm: Vec<u8>,
        cancel: Arc<CancelRegistry>,
    ) -> Result<super::GroqChunkTranscript, BoundaryError> {
        tokio::task::spawn_blocking(move || {
            request_groq_chunk(credential, endpoint, &params, pcm, &cancel)
        })
        .await
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "Groq request task failed"))?
    }
}

/// Builds the curl `--config` body for a Groq transcription request: the audio
/// form part plus the accuracy gains — `model`, `language`, `temperature=0`,
/// `response_format`, and (when non-empty) the vocabulary `prompt`. Since
/// slice B4 the request asks for `verbose_json` with BOTH timestamp
/// granularities (`word` for the word list, `segment` for the per-segment
/// `avg_logprob` the word confidences are derived from) — the documented
/// OpenAI-compatible Whisper shape. The parser tolerates a server that
/// ignores the format and returns plain JSON: the text is still extracted and
/// the missing word list simply yields no confidence evidence. Rejects a
/// model or language carrying control characters; a control-character-bearing
/// prompt is defensively stripped rather than rejected. Kept pure and separate
/// from the request so the request shape is testable without a network call.
pub(super) fn build_groq_curl_config(
    endpoint: &str,
    credential: &Credential,
    file_path: &str,
    params: &GroqRequestParams,
) -> Result<String, BoundaryError> {
    if params.model.is_empty() || params.model.contains(['\n', '\r']) {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "invalid Groq model",
        ));
    }
    if params.language.contains(['\n', '\r']) {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "invalid Groq language",
        ));
    }
    let endpoint = curl_config_escape(endpoint);
    let credential = curl_config_escape(credential.expose_to_boundary());
    let path = curl_config_escape(file_path);
    let model = curl_config_escape(&params.model);
    let mut config = format!(
        "url = \"{endpoint}\"\nheader = \"Authorization: Bearer {credential}\"\nform = \"file=@{path};filename=recording.flac;type=audio/flac\"\nform = \"model={model}\"\nform = \"response_format=verbose_json\"\nform = \"timestamp_granularities[]=word\"\nform = \"timestamp_granularities[]=segment\"\nform = \"temperature=0\"\n"
    );
    if !params.language.is_empty() {
        let language = curl_config_escape(&params.language);
        config.push_str(&format!("form = \"language={language}\"\n"));
    }
    let prompt: String = params
        .prompt
        .chars()
        .filter(|c| *c != '\n' && *c != '\r')
        .collect();
    if !prompt.is_empty() {
        let prompt = curl_config_escape(&prompt);
        config.push_str(&format!("form = \"prompt={prompt}\"\n"));
    }
    Ok(config)
}
