//! Catalog HTTPS fetch. Ignores proxy environment. No free-form caller URLs.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rustls::pki_types::ServerName;
use sha2::{Digest, Sha256};

use super::url_policy::{
    CatalogUrl, UrlPolicyError, check_url, resolve_redirect, validate_catalog_url,
};
use super::{INSTALL_DEADLINE, NO_PROGRESS};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FetchError {
    Policy(UrlPolicyError),
    RedirectEscape {
        host: String,
    },
    Truncated {
        expected: u64,
        actual: u64,
    },
    Oversized {
        expected: u64,
        actual: u64,
    },
    Status(u16),
    /// The server answered a Range request with the whole file (200). Nothing
    /// was written; the caller starts the download again from byte 0.
    RangeIgnored,
    Io(String),
    NoSpace,
    Deadline,
    NoProgress,
}

impl From<UrlPolicyError> for FetchError {
    fn from(value: UrlPolicyError) -> Self {
        if let UrlPolicyError::HostNotCataloged { host } = value {
            Self::RedirectEscape { host }
        } else {
            Self::Policy(value)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchRequest {
    pub url: String,
    pub allowed_hosts: Vec<String>,
    pub expected_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchResponse {
    pub body: Vec<u8>,
    pub hops: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchedSummary {
    pub bytes: u64,
    pub sha256_hex: String,
    pub hops: u8,
}

impl FetchedSummary {
    pub fn matches_pin(&self, expected_hex: &str, expected_bytes: u64) -> bool {
        self.bytes == expected_bytes
            && expected_hex.len() == 64
            && expected_hex.bytes().all(|b| b.is_ascii_hexdigit())
            && expected_hex.bytes().any(|b| b != b'0')
            && self.sha256_hex == expected_hex
    }
}

pub trait ArtifactFetcher {
    fn fetch(&self, request: &FetchRequest) -> Result<FetchResponse, FetchError>;

    /// The sink may contain partial bytes on failure; callers must keep it private.
    fn fetch_to(
        &self,
        request: &FetchRequest,
        sink: &mut dyn Write,
    ) -> Result<FetchedSummary, FetchError> {
        let fetched = self.fetch(request)?;
        let mut summary = stream_body(
            &mut fetched.body.as_slice(),
            sink,
            request.expected_bytes,
            Instant::now(),
        )?;
        summary.hops = fetched.hops;
        Ok(summary)
    }

    /// Resume a download whose first `held` bytes are already in the sink;
    /// `prefix` is their running SHA-256. The sink receives only the remaining
    /// bytes, and the summary covers the whole file. A fetcher without Range
    /// support returns [`FetchError::RangeIgnored`] before writing anything.
    fn fetch_range_to(
        &self,
        _request: &FetchRequest,
        _held: u64,
        _prefix: &Sha256,
        _sink: &mut dyn Write,
    ) -> Result<FetchedSummary, FetchError> {
        Err(FetchError::RangeIgnored)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProductionHttps;

impl ArtifactFetcher for ProductionHttps {
    fn fetch(&self, request: &FetchRequest) -> Result<FetchResponse, FetchError> {
        let mut body = Vec::new();
        let summary = self.fetch_to(request, &mut body)?;
        Ok(FetchResponse {
            body,
            hops: summary.hops,
        })
    }

    fn fetch_to(
        &self,
        request: &FetchRequest,
        sink: &mut dyn Write,
    ) -> Result<FetchedSummary, FetchError> {
        ensure_ring_provider();
        let started = Instant::now();
        let (mut summary, hops) =
            follow_redirects(request, |url| https_get(url, request, None, sink, started))?;
        summary.hops = hops;
        Ok(summary)
    }

    fn fetch_range_to(
        &self,
        request: &FetchRequest,
        held: u64,
        prefix: &Sha256,
        sink: &mut dyn Write,
    ) -> Result<FetchedSummary, FetchError> {
        ensure_ring_provider();
        let started = Instant::now();
        let (mut summary, hops) = follow_redirects(request, |url| {
            https_get(url, request, Some((held, prefix)), sink, started)
        })?;
        summary.hops = hops;
        Ok(summary)
    }
}

fn ensure_ring_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn follow_catalog_redirects<F>(
    request: &FetchRequest,
    mut hop: F,
) -> Result<FetchResponse, FetchError>
where
    F: FnMut(&CatalogUrl) -> Result<Hop, FetchError>,
{
    let (body, hops) = follow_redirects(request, |url| match hop(url)? {
        Hop::Redirect { location } => Ok(HttpHop::Redirect { location }),
        Hop::Body(body) => {
            check_size(body.len() as u64, request.expected_bytes)?;
            Ok(HttpHop::Body(body))
        }
    })?;
    Ok(FetchResponse { body, hops })
}

fn follow_redirects<T>(
    request: &FetchRequest,
    mut hop: impl FnMut(&CatalogUrl) -> Result<HttpHop<T>, FetchError>,
) -> Result<(T, u8), FetchError> {
    let hosts: Vec<&str> = request.allowed_hosts.iter().map(String::as_str).collect();
    let mut current = validate_catalog_url(&request.url, &hosts)?;
    let mut hops = 0_u8;
    loop {
        match hop(&current)? {
            HttpHop::Redirect { location } => {
                current = resolve_redirect(&current.https, &location, &hosts, hops)?;
                hops = hops.saturating_add(1);
            }
            HttpHop::Body(body) => return Ok((body, hops)),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum HttpHop<T> {
    Redirect { location: String },
    Body(T),
}

fn check_size(actual: u64, expected: u64) -> Result<(), FetchError> {
    if actual < expected {
        Err(FetchError::Truncated { expected, actual })
    } else if actual > expected {
        Err(FetchError::Oversized { expected, actual })
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Hop {
    Redirect { location: String },
    Body(Vec<u8>),
}

#[derive(Clone, Debug)]
pub enum ScriptedHop {
    Redirect(String),
    Body(Vec<u8>),
    Truncated(Vec<u8>),
    /// A 206 answer to a Range request: the bytes after the held prefix.
    Partial(Vec<u8>),
    NoSpace,
    Io(ErrorKind),
}

#[derive(Debug, Default)]
pub struct ScriptedFetcher {
    hops: Mutex<VecDeque<ScriptedHop>>,
}

impl ScriptedFetcher {
    #[must_use]
    pub fn new(hops: Vec<ScriptedHop>) -> Self {
        Self {
            hops: Mutex::new(hops.into()),
        }
    }
}

impl ArtifactFetcher for ScriptedFetcher {
    fn fetch(&self, request: &FetchRequest) -> Result<FetchResponse, FetchError> {
        follow_catalog_redirects(request, |_| {
            let hop = self
                .hops
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front();
            match hop {
                Some(ScriptedHop::Redirect(location)) => Ok(Hop::Redirect { location }),
                Some(ScriptedHop::Body(body)) => Ok(Hop::Body(body)),
                Some(ScriptedHop::Truncated(body)) => Ok(Hop::Body(body)),
                Some(ScriptedHop::Partial(_)) => Err(FetchError::Status(206)),
                Some(ScriptedHop::NoSpace) => Err(FetchError::NoSpace),
                Some(ScriptedHop::Io(kind)) => Err(FetchError::Io(kind.to_string())),
                None => Err(FetchError::Io("script exhausted".into())),
            }
        })
    }

    /// Streams like production: a truncated body writes its bytes, then fails.
    fn fetch_to(
        &self,
        request: &FetchRequest,
        sink: &mut dyn Write,
    ) -> Result<FetchedSummary, FetchError> {
        let (mut summary, hops) = follow_redirects(request, |_| {
            let hop = self
                .hops
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front();
            match hop {
                Some(ScriptedHop::Redirect(location)) => Ok(HttpHop::Redirect { location }),
                Some(ScriptedHop::Body(body) | ScriptedHop::Truncated(body)) => {
                    Ok(HttpHop::Body(stream_body(
                        &mut body.as_slice(),
                        sink,
                        request.expected_bytes,
                        Instant::now(),
                    )?))
                }
                Some(ScriptedHop::Partial(_)) => Err(FetchError::Status(206)),
                Some(ScriptedHop::NoSpace) => Err(FetchError::NoSpace),
                Some(ScriptedHop::Io(kind)) => Err(FetchError::Io(kind.to_string())),
                None => Err(FetchError::Io("script exhausted".into())),
            }
        })?;
        summary.hops = hops;
        Ok(summary)
    }

    fn fetch_range_to(
        &self,
        request: &FetchRequest,
        held: u64,
        prefix: &Sha256,
        sink: &mut dyn Write,
    ) -> Result<FetchedSummary, FetchError> {
        let (mut summary, hops) = follow_redirects(request, |_| {
            let mut hops = self
                .hops
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match hops.pop_front() {
                Some(ScriptedHop::Redirect(location)) => Ok(HttpHop::Redirect { location }),
                Some(ScriptedHop::Partial(tail)) => {
                    drop(hops);
                    Ok(HttpHop::Body(stream_body_from(
                        &mut tail.as_slice(),
                        sink,
                        request.expected_bytes,
                        Instant::now(),
                        held,
                        prefix.clone(),
                    )?))
                }
                // A whole body answers the Range request with 200: leave it
                // for the full fetch that follows.
                Some(hop @ (ScriptedHop::Body(_) | ScriptedHop::Truncated(_))) => {
                    hops.push_front(hop);
                    Err(FetchError::RangeIgnored)
                }
                Some(ScriptedHop::NoSpace) => Err(FetchError::NoSpace),
                Some(ScriptedHop::Io(kind)) => Err(FetchError::Io(kind.to_string())),
                None => Err(FetchError::Io("script exhausted".into())),
            }
        })?;
        summary.hops = hops;
        Ok(summary)
    }
}

fn https_get(
    url: &CatalogUrl,
    request: &FetchRequest,
    resume: Option<(u64, &Sha256)>,
    sink: &mut dyn Write,
    started: Instant,
) -> Result<HttpHop<FetchedSummary>, FetchError> {
    // Proxy environment is never read. Direct HTTPS to the catalog host only.
    if started.elapsed() > INSTALL_DEADLINE {
        return Err(FetchError::Deadline);
    }
    let host = url
        .https
        .host_str()
        .ok_or(FetchError::Policy(UrlPolicyError::MissingHost))?
        .to_owned();
    check_url(&url.https, &[&host]).map_err(FetchError::from)?;
    let server_name = ServerName::try_from(host.clone())
        .map_err(|_| FetchError::Policy(UrlPolicyError::MissingHost))?;
    let tcp = connect_https(&host, started)?;
    tcp.set_read_timeout(Some(NO_PROGRESS)).map_err(io_err)?;
    tcp.set_write_timeout(Some(NO_PROGRESS)).map_err(io_err)?;
    ensure_ring_provider();
    let conn = rustls::ClientConnection::new(https_config(), server_name).map_err(tls_err)?;
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    let path = if url.https.path().is_empty() {
        "/"
    } else {
        url.https.path()
    };
    let query = url
        .https
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let range = resume
        .map(|(held, _)| format!("Range: bytes={held}-\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET {path}{query} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: voisu-l3-catalog\r\n{range}\r\n"
    )
    .map_err(io_err)?;
    stream.flush().map_err(io_err)?;
    read_http_hop_from(
        &mut stream,
        sink,
        request.expected_bytes,
        started,
        resume.map(|(held, prefix)| (held, prefix.clone())),
    )
}

/// `resume` is the held prefix of a Range request: its length and hash.
fn read_http_hop_from<R: Read>(
    stream: &mut R,
    sink: &mut dyn Write,
    expected_bytes: u64,
    started: Instant,
    resume: Option<(u64, Sha256)>,
) -> Result<HttpHop<FetchedSummary>, FetchError> {
    let mut reader = BufReader::with_capacity(CHUNK_BYTES, stream);
    let mut headers = Vec::new();
    // Consume only headers. Buffered body bytes stay bounded and redirects never
    // touch the sink, even when their response includes a body.
    loop {
        check_deadline(started)?;
        let available = reader.fill_buf().map_err(io_err)?;
        if available.is_empty() {
            return Err(FetchError::Io("truncated HTTP headers".into()));
        }
        let mut consumed = 0;
        for byte in available {
            headers.push(*byte);
            consumed += 1;
            if headers.len() > MAX_HEADER_BYTES {
                return Err(FetchError::Io("HTTP headers exceed cap".into()));
            }
            if headers.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        reader.consume(consumed);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    match (parse_http_headers(&headers)?, resume) {
        (HttpHop::Redirect { location }, _) => Ok(HttpHop::Redirect { location }),
        (HttpHop::Body(BodyRange::Whole), None) => Ok(HttpHop::Body(stream_body(
            &mut reader,
            sink,
            expected_bytes,
            started,
        )?)),
        (HttpHop::Body(BodyRange::Whole), Some(_)) => Err(FetchError::RangeIgnored),
        // The partial body must continue exactly where the held bytes end and
        // run to the pinned size; anything else is not this file's tail.
        (HttpHop::Body(BodyRange::Partial { first, last, total }), Some((held, prefix)))
            if first == held && total == expected_bytes && last.checked_add(1) == Some(total) =>
        {
            Ok(HttpHop::Body(stream_body_from(
                &mut reader,
                sink,
                expected_bytes,
                started,
                held,
                prefix,
            )?))
        }
        (HttpHop::Body(BodyRange::Partial { .. }), _) => {
            Err(FetchError::Io("unexpected HTTP Content-Range".into()))
        }
    }
}

/// Which bytes of the file a 2xx response carries.
#[derive(Debug, Eq, PartialEq)]
enum BodyRange {
    Whole,
    Partial { first: u64, last: u64, total: u64 },
}

/// `bytes FIRST-LAST/TOTAL`; an unknown total (`*`) cannot be checked.
fn parse_content_range(value: &str) -> Option<BodyRange> {
    let (first, rest) = value.trim().strip_prefix("bytes ")?.split_once('-')?;
    let (last, total) = rest.split_once('/')?;
    let number = |text: &str| -> Option<u64> {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    let (first, last, total) = (number(first)?, number(last)?, number(total)?);
    (first <= last && last < total).then_some(BodyRange::Partial { first, last, total })
}

fn check_deadline(started: Instant) -> Result<(), FetchError> {
    if started.elapsed() > INSTALL_DEADLINE {
        Err(FetchError::Deadline)
    } else {
        Ok(())
    }
}

fn stream_body<R: Read>(
    reader: &mut R,
    sink: &mut dyn Write,
    expected_bytes: u64,
    started: Instant,
) -> Result<FetchedSummary, FetchError> {
    stream_body_from(reader, sink, expected_bytes, started, 0, Sha256::new())
}

/// Streams the bytes after a held prefix of `held` bytes whose running hash
/// is `hash`. Sizes and the digest cover the whole file.
fn stream_body_from<R: Read>(
    reader: &mut R,
    sink: &mut dyn Write,
    expected_bytes: u64,
    started: Instant,
    held: u64,
    mut hash: Sha256,
) -> Result<FetchedSummary, FetchError> {
    let mut bytes = held;
    let mut chunk = [0_u8; CHUNK_BYTES];
    loop {
        check_deadline(started)?;
        // Probe at most one byte beyond the pin, and never write that byte.
        let limit = expected_bytes.saturating_sub(bytes).saturating_add(1);
        let limit = usize::try_from(limit.min(CHUNK_BYTES as u64)).expect("chunk fits usize");
        let n = reader.read(&mut chunk[..limit]).map_err(io_err)?;
        check_deadline(started)?;
        if n == 0 {
            check_size(bytes, expected_bytes)?;
            return Ok(FetchedSummary {
                bytes,
                sha256_hex: format!("{:x}", hash.finalize()),
                hops: 0,
            });
        }
        let actual = bytes.saturating_add(n as u64);
        if actual > expected_bytes {
            return Err(FetchError::Oversized {
                expected: expected_bytes,
                actual,
            });
        }
        let write_started = Instant::now();
        sink.write_all(&chunk[..n]).map_err(io_err)?;
        if write_started.elapsed() > NO_PROGRESS {
            return Err(FetchError::NoProgress);
        }
        check_deadline(started)?;
        hash.update(&chunk[..n]);
        bytes = actual;
    }
}

fn connect_https(host: &str, started: Instant) -> Result<TcpStream, FetchError> {
    let addrs = (host, 443).to_socket_addrs().map_err(io_err)?;
    let mut last = None;
    for addr in addrs {
        if started.elapsed() > INSTALL_DEADLINE {
            return Err(FetchError::Deadline);
        }
        match TcpStream::connect_timeout(&addr, NO_PROGRESS) {
            Ok(tcp) => return Ok(tcp),
            Err(error) if error.kind() == ErrorKind::TimedOut => {
                last = Some(FetchError::NoProgress);
            }
            Err(error) => last = Some(io_err(error)),
        }
    }
    Err(last.unwrap_or_else(|| FetchError::Io("no HTTPS addresses".into())))
}

fn https_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

fn parse_http_headers(headers: &[u8]) -> Result<HttpHop<BodyRange>, FetchError> {
    let text = std::str::from_utf8(headers)
        .map_err(|_| FetchError::Io("HTTP headers are not UTF-8".into()))?;
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| FetchError::Io("invalid HTTP status".into()))?;
    let mut location = None;
    let mut transfer_encoding = false;
    let mut content_range = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("location") {
            location = Some(value.trim().to_owned());
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            transfer_encoding = true;
        } else if name.eq_ignore_ascii_case("content-range") {
            content_range = Some(value.to_owned());
        }
    }
    match status {
        200 | 206 => {
            // Chunk framing was previously treated as payload. Never publish it.
            if transfer_encoding {
                return Err(FetchError::Io("unsupported HTTP Transfer-Encoding".into()));
            }
            // As before, Connection: close delimits the body even when a
            // Content-Length is present; the pin is checked against actual bytes.
            if status == 200 {
                return Ok(HttpHop::Body(BodyRange::Whole));
            }
            content_range
                .as_deref()
                .and_then(parse_content_range)
                .map(HttpHop::Body)
                .ok_or_else(|| FetchError::Io("unexpected HTTP Content-Range".into()))
        }
        301 | 302 | 303 | 307 | 308 => {
            let location =
                location.ok_or_else(|| FetchError::Io("redirect without Location".into()))?;
            Ok(HttpHop::Redirect { location })
        }
        other => Err(FetchError::Status(other)),
    }
}

pub(super) fn io_err(error: io::Error) -> FetchError {
    if error.raw_os_error() == Some(libc::ENOSPC) {
        FetchError::NoSpace
    } else if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) {
        FetchError::NoProgress
    } else {
        FetchError::Io(error.to_string())
    }
}

fn tls_err(error: rustls::Error) -> FetchError {
    FetchError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str) -> FetchRequest {
        FetchRequest {
            url: url.to_owned(),
            allowed_hosts: vec!["fixtures.voisu.test".into()],
            expected_bytes: 4,
        }
    }

    #[test]
    fn scripted_redirect_escape_is_rejected() {
        let fetcher = ScriptedFetcher::new(vec![ScriptedHop::Redirect(
            "https://evil.example/steal".to_owned(),
        )]);
        let error = fetcher
            .fetch(&request("https://fixtures.voisu.test/model.bin"))
            .unwrap_err();
        assert!(matches!(error, FetchError::RedirectEscape { host } if host == "evil.example"));
    }

    #[test]
    fn truncation_is_visible() {
        let fetcher = ScriptedFetcher::new(vec![ScriptedHop::Truncated(b"ab".to_vec())]);
        let error = fetcher
            .fetch(&request("https://fixtures.voisu.test/model.bin"))
            .unwrap_err();
        assert!(matches!(
            error,
            FetchError::Truncated {
                expected: 4,
                actual: 2
            }
        ));
    }

    #[test]
    fn enospc_is_not_a_successful_body() {
        let fetcher = ScriptedFetcher::new(vec![ScriptedHop::NoSpace]);
        assert_eq!(
            fetcher
                .fetch(&request("https://fixtures.voisu.test/model.bin"))
                .unwrap_err(),
            FetchError::NoSpace
        );
    }

    #[test]
    fn production_client_never_reads_proxy_env() {
        let production = include_str!("fetch.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        assert!(!production.contains("var_os"));
        assert!(!production.contains("std::env"));
        assert!(production.contains("Proxy environment is never read"));
        assert!(production.contains("install_default"));
        assert!(production.contains("set_read_timeout"));
        assert!(production.contains("connect_timeout"));
        assert!(production.contains("NO_PROGRESS"));
        assert!(production.contains("INSTALL_DEADLINE"));
    }

    fn read_http_hop<R: Read>(
        stream: &mut R,
        sink: &mut dyn Write,
        expected_bytes: u64,
        started: Instant,
    ) -> Result<HttpHop<FetchedSummary>, FetchError> {
        read_http_hop_from(stream, sink, expected_bytes, started, None)
    }

    // The old raw parser tests now exercise the reader/sink boundary.
    fn parse_http_hop(mut raw: &[u8], expected_bytes: u64) -> Result<Hop, FetchError> {
        let mut body = Vec::new();
        match read_http_hop(&mut raw, &mut body, expected_bytes, Instant::now())? {
            HttpHop::Redirect { location } => Ok(Hop::Redirect { location }),
            HttpHop::Body(_) => Ok(Hop::Body(body)),
        }
    }

    #[test]
    fn http_parse_redirect_and_ok() {
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: /b\r\n\r\n";
        assert_eq!(
            parse_http_hop(redirect, 4).unwrap(),
            Hop::Redirect {
                location: "/b".into()
            }
        );
        let ok = b"HTTP/1.1 200 OK\r\n\r\nabcd";
        assert_eq!(parse_http_hop(ok, 4).unwrap(), Hop::Body(b"abcd".to_vec()));
        assert!(matches!(
            parse_http_hop(b"HTTP/1.1 200 OK\r\n\r\nabcde", 4),
            Err(FetchError::Oversized {
                expected: 4,
                actual: 5
            })
        ));
    }

    #[test]
    fn capped_read_rejects_body_past_expected() {
        let started = Instant::now();
        let payload = b"HTTP/1.1 200 OK\r\n\r\n0123456789";
        let error =
            read_http_hop(&mut payload.as_slice(), &mut Vec::new(), 4, started).unwrap_err();
        assert!(matches!(error, FetchError::Oversized { expected: 4, .. }));
    }

    struct GeneratedBody {
        remaining: usize,
        consumed: usize,
    }

    impl Read for GeneratedBody {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            assert!(buf.len() <= CHUNK_BYTES, "unbounded read requested");
            let len = self.remaining.min(buf.len());
            buf[..len].fill(b'x');
            self.remaining -= len;
            self.consumed += len;
            Ok(len)
        }
    }

    #[derive(Default)]
    struct CheckingSink {
        bytes: usize,
        writes: usize,
    }

    impl Write for CheckingSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            assert!(buf.len() <= CHUNK_BYTES, "unbounded write requested");
            assert!(buf.iter().all(|byte| *byte == b'x'));
            self.bytes += buf.len();
            self.writes += 1;
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn http_streams_generated_body_in_bounded_chunks_with_digest() {
        let len = CHUNK_BYTES * 5 + 123;
        for content_length in [String::new(), format!("Content-Length: {len}\r\n")] {
            let headers = format!("HTTP/1.1 200 OK\r\n{content_length}\r\n");
            let body = GeneratedBody {
                remaining: len,
                consumed: 0,
            };
            let mut reader = headers.as_bytes().chain(body);
            let mut sink = CheckingSink::default();
            let HttpHop::Body(summary) =
                read_http_hop(&mut reader, &mut sink, len as u64, Instant::now()).unwrap()
            else {
                panic!("expected body");
            };
            assert_eq!(sink.bytes, len);
            assert!(sink.writes > 5);
            assert_eq!(reader.get_ref().1.consumed, len);
            let mut expected = Sha256::new();
            for _ in 0..5 {
                expected.update([b'x'; CHUNK_BYTES]);
            }
            expected.update([b'x'; 123]);
            assert_eq!(summary.bytes, len as u64);
            assert_eq!(summary.sha256_hex, format!("{:x}", expected.finalize()));
        }
    }

    #[test]
    fn oversized_stream_stops_at_first_extra_byte_without_writing_it() {
        let expected = CHUNK_BYTES + 7;
        let mut reader = GeneratedBody {
            remaining: CHUNK_BYTES * 10,
            consumed: 0,
        };
        let mut sink = CheckingSink::default();
        assert_eq!(
            stream_body(&mut reader, &mut sink, expected as u64, Instant::now()),
            Err(FetchError::Oversized {
                expected: expected as u64,
                actual: expected as u64 + 1
            })
        );
        assert_eq!(reader.consumed, expected + 1);
        assert_eq!(sink.bytes, CHUNK_BYTES);
    }

    #[test]
    fn truncated_stream_reports_actual_bytes() {
        let mut reader = GeneratedBody {
            remaining: CHUNK_BYTES + 3,
            consumed: 0,
        };
        let mut sink = CheckingSink::default();
        assert_eq!(
            stream_body(
                &mut reader,
                &mut sink,
                (CHUNK_BYTES * 2) as u64,
                Instant::now()
            ),
            Err(FetchError::Truncated {
                expected: (CHUNK_BYTES * 2) as u64,
                actual: (CHUNK_BYTES + 3) as u64
            })
        );
        assert_eq!(sink.bytes, CHUNK_BYTES + 3);
    }

    #[test]
    fn redirect_body_never_reaches_sink_and_summary_keeps_hops() {
        let request = request("https://fixtures.voisu.test/a");
        let mut sink = Vec::new();
        let mut count = 0;
        let (summary, hops) = follow_redirects(&request, |_| {
            count += 1;
            let mut raw: &[u8] = if count == 1 {
                b"HTTP/1.1 302 Found\r\nLocation: /b\r\nContent-Length: 100\r\n\r\nignored body"
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nabcd"
            };
            read_http_hop(&mut raw, &mut sink, 4, Instant::now())
        })
        .unwrap();
        assert_eq!(hops, 1);
        assert_eq!(sink, b"abcd");
        assert!(summary.matches_pin(&super::super::sha256_hex(b"abcd"), 4));
        let fetcher = ScriptedFetcher::new(vec![
            ScriptedHop::Redirect("/b".into()),
            ScriptedHop::Body(b"abcd".to_vec()),
        ]);
        let summary = fetcher.fetch_to(&request, &mut io::sink()).unwrap();
        assert_eq!(summary.hops, 1);
    }

    fn prefix_hash(bytes: &[u8]) -> Sha256 {
        let mut hash = Sha256::new();
        hash.update(bytes);
        hash
    }

    #[test]
    fn a_206_tail_resumes_after_the_held_prefix_and_hashes_the_whole_file() {
        let mut raw = &b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-3/4\r\n\r\ncd"[..];
        let mut sink = Vec::new();
        let HttpHop::Body(summary) = read_http_hop_from(
            &mut raw,
            &mut sink,
            4,
            Instant::now(),
            Some((2, prefix_hash(b"ab"))),
        )
        .unwrap() else {
            panic!("expected body");
        };
        assert_eq!(sink, b"cd");
        assert!(summary.matches_pin(&super::super::sha256_hex(b"abcd"), 4));
    }

    #[test]
    fn a_range_answer_that_is_not_the_held_tail_writes_nothing() {
        for raw in [
            // The server sent the whole file: the caller restarts from byte 0.
            &b"HTTP/1.1 200 OK\r\n\r\nabcd"[..],
            // A tail that starts elsewhere, ends early, or has another total.
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-3/4\r\n\r\nbcd",
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-2/4\r\n\r\nc",
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-4/5\r\n\r\ncde",
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-3/*\r\n\r\ncd",
            b"HTTP/1.1 206 Partial Content\r\n\r\ncd",
        ] {
            let mut sink = Vec::new();
            let error = read_http_hop_from(
                &mut &raw[..],
                &mut sink,
                4,
                Instant::now(),
                Some((2, prefix_hash(b"ab"))),
            )
            .unwrap_err();
            let expected = if raw.starts_with(b"HTTP/1.1 200") {
                FetchError::RangeIgnored
            } else {
                FetchError::Io("unexpected HTTP Content-Range".into())
            };
            assert_eq!(error, expected, "{}", String::from_utf8_lossy(raw));
            assert!(sink.is_empty());
        }
        // An unrequested 206 is never taken for the whole file.
        assert_eq!(
            parse_http_hop(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/4\r\n\r\nabcd",
                4
            ),
            Err(FetchError::Io("unexpected HTTP Content-Range".into()))
        );
    }

    #[test]
    fn scripted_range_fetch_resumes_or_leaves_a_whole_body_for_the_restart() {
        let request = request("https://fixtures.voisu.test/a");
        let fetcher = ScriptedFetcher::new(vec![ScriptedHop::Partial(b"cd".to_vec())]);
        let mut sink = Vec::new();
        let summary = fetcher
            .fetch_range_to(&request, 2, &prefix_hash(b"ab"), &mut sink)
            .unwrap();
        assert_eq!(sink, b"cd");
        assert!(summary.matches_pin(&super::super::sha256_hex(b"abcd"), 4));

        let fetcher = ScriptedFetcher::new(vec![ScriptedHop::Body(b"abcd".to_vec())]);
        assert_eq!(
            fetcher.fetch_range_to(&request, 2, &prefix_hash(b"ab"), &mut sink),
            Err(FetchError::RangeIgnored)
        );
        assert_eq!(fetcher.fetch(&request).unwrap().body, b"abcd");
    }

    #[test]
    fn chunked_is_rejected_before_writing() {
        let mut raw =
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n0\r\n\r\n"[..];
        let mut sink = Vec::new();
        assert_eq!(
            read_http_hop(&mut raw, &mut sink, 4, Instant::now()),
            Err(FetchError::Io("unsupported HTTP Transfer-Encoding".into()))
        );
        assert!(sink.is_empty());
    }

    #[test]
    fn headers_are_capped_and_deadline_precedes_read() {
        let mut headers = b"HTTP/1.1 200 OK\r\nX: ".to_vec();
        headers.extend(std::iter::repeat_n(b'x', MAX_HEADER_BYTES));
        assert_eq!(
            read_http_hop(&mut headers.as_slice(), &mut io::sink(), 4, Instant::now()),
            Err(FetchError::Io("HTTP headers exceed cap".into()))
        );
        assert_eq!(
            read_http_hop(
                &mut &b""[..],
                &mut io::sink(),
                4,
                Instant::now() - INSTALL_DEADLINE - std::time::Duration::from_secs(1)
            ),
            Err(FetchError::Deadline)
        );
    }

    #[test]
    fn streaming_sink_enospc_and_read_stalls_keep_error_classification() {
        struct FullDisk;
        impl Write for FullDisk {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            stream_body(&mut &b"abcd"[..], &mut FullDisk, 4, Instant::now()),
            Err(FetchError::NoSpace)
        );
        struct Stalled(ErrorKind);
        impl Read for Stalled {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(self.0.into())
            }
        }
        for kind in [ErrorKind::WouldBlock, ErrorKind::TimedOut] {
            assert_eq!(
                read_http_hop(&mut Stalled(kind), &mut io::sink(), 4, Instant::now()),
                Err(FetchError::NoProgress)
            );
            assert_eq!(
                stream_body(&mut Stalled(kind), &mut io::sink(), 4, Instant::now()),
                Err(FetchError::NoProgress)
            );
        }
    }
}
