//! Catalog HTTPS fetch. Ignores proxy environment. No free-form caller URLs.

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rustls::pki_types::ServerName;

use super::url_policy::{
    CatalogUrl, UrlPolicyError, check_url, resolve_redirect, validate_catalog_url,
};
use super::{INSTALL_DEADLINE, NO_PROGRESS};

const MAX_HEADER_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FetchError {
    Policy(UrlPolicyError),
    RedirectEscape { host: String },
    Truncated { expected: u64, actual: u64 },
    Oversized { expected: u64, actual: u64 },
    Status(u16),
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

pub trait ArtifactFetcher {
    fn fetch(&self, request: &FetchRequest) -> Result<FetchResponse, FetchError>;
}

#[derive(Clone, Debug, Default)]
pub struct ProductionHttps;

impl ArtifactFetcher for ProductionHttps {
    fn fetch(&self, request: &FetchRequest) -> Result<FetchResponse, FetchError> {
        ensure_ring_provider();
        let started = Instant::now();
        follow_catalog_redirects(request, |url| https_get(url, request, started))
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
    let hosts: Vec<&str> = request.allowed_hosts.iter().map(String::as_str).collect();
    let mut current = validate_catalog_url(&request.url, &hosts)?;
    let mut hops = 0_u8;
    loop {
        match hop(&current)? {
            Hop::Redirect { location } => {
                current = resolve_redirect(&current.https, &location, &hosts, hops)?;
                hops = hops.saturating_add(1);
            }
            Hop::Body(body) => {
                let actual = body.len() as u64;
                if actual < request.expected_bytes {
                    return Err(FetchError::Truncated {
                        expected: request.expected_bytes,
                        actual,
                    });
                }
                if actual > request.expected_bytes {
                    return Err(FetchError::Oversized {
                        expected: request.expected_bytes,
                        actual,
                    });
                }
                return Ok(FetchResponse { body, hops });
            }
        }
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
                Some(ScriptedHop::NoSpace) => Err(FetchError::NoSpace),
                Some(ScriptedHop::Io(kind)) => Err(FetchError::Io(kind.to_string())),
                None => Err(FetchError::Io("script exhausted".into())),
            }
        })
    }
}

fn https_get(
    url: &CatalogUrl,
    request: &FetchRequest,
    started: Instant,
) -> Result<Hop, FetchError> {
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
    write!(
        stream,
        "GET {path}{query} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: voisu-l3-catalog\r\n\r\n"
    )
    .map_err(io_err)?;
    stream.flush().map_err(io_err)?;
    let raw = read_capped_http(&mut stream, request.expected_bytes, started)?;
    parse_http_hop(&raw, request.expected_bytes)
}

fn read_capped_http<R: Read>(
    stream: &mut R,
    expected_bytes: u64,
    started: Instant,
) -> Result<Vec<u8>, FetchError> {
    let body_cap = usize::try_from(expected_bytes.saturating_add(1)).unwrap_or(usize::MAX);
    let total_cap = MAX_HEADER_BYTES.saturating_add(body_cap);
    let mut raw = Vec::new();
    let mut buf = [0_u8; 4096];
    let mut headers_done = false;
    loop {
        if started.elapsed() > INSTALL_DEADLINE {
            return Err(FetchError::Deadline);
        }
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error)
                if error.kind() == ErrorKind::TimedOut || error.kind() == ErrorKind::WouldBlock =>
            {
                return Err(FetchError::NoProgress);
            }
            Err(error) => return Err(io_err(error)),
        };
        raw.extend_from_slice(&buf[..n]);
        if !headers_done {
            if let Some(pos) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                if pos + 4 > MAX_HEADER_BYTES {
                    return Err(FetchError::Io("HTTP headers exceed cap".into()));
                }
                headers_done = true;
                let body_len = raw.len() - (pos + 4);
                if body_len > body_cap {
                    return Err(FetchError::Oversized {
                        expected: expected_bytes,
                        actual: body_len as u64,
                    });
                }
            } else if raw.len() > MAX_HEADER_BYTES {
                return Err(FetchError::Io("HTTP headers exceed cap".into()));
            }
        } else if raw.len() > total_cap {
            return Err(FetchError::Oversized {
                expected: expected_bytes,
                actual: raw.len() as u64,
            });
        }
    }
    Ok(raw)
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

fn parse_http_hop(raw: &[u8], expected_bytes: u64) -> Result<Hop, FetchError> {
    let header_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| FetchError::Io("truncated HTTP headers".into()))?;
    if header_end + 4 > MAX_HEADER_BYTES {
        return Err(FetchError::Io("HTTP headers exceed cap".into()));
    }
    let (headers, body) = raw.split_at(header_end + 4);
    let text = std::str::from_utf8(headers)
        .map_err(|_| FetchError::Io("HTTP headers are not UTF-8".into()))?;
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| FetchError::Io("invalid HTTP status".into()))?;
    let mut location = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("location") {
            location = Some(value.trim().to_owned());
        }
    }
    match status {
        200 => {
            if body.len() as u64 > expected_bytes {
                return Err(FetchError::Oversized {
                    expected: expected_bytes,
                    actual: body.len() as u64,
                });
            }
            Ok(Hop::Body(body.to_vec()))
        }
        301 | 302 | 303 | 307 | 308 => {
            let location =
                location.ok_or_else(|| FetchError::Io("redirect without Location".into()))?;
            Ok(Hop::Redirect { location })
        }
        other => Err(FetchError::Status(other)),
    }
}

fn io_err(error: io::Error) -> FetchError {
    if error.raw_os_error() == Some(libc::ENOSPC) {
        FetchError::NoSpace
    } else if error.kind() == ErrorKind::TimedOut {
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
        let error = read_capped_http(&mut payload.as_slice(), 4, started).unwrap_err();
        assert!(matches!(error, FetchError::Oversized { expected: 4, .. }));
    }
}
