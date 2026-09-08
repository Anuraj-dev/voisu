//! Structural HTTPS URL policy for catalog downloads.
//!
//! HTTPS only, no userinfo, at most three redirects, every hop on cataloged
//! hosts. This is not the Groq string-prefix allowlist.

use url::Url;

pub const MAX_REDIRECTS: u8 = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UrlPolicyError {
    NotHttps,
    Userinfo,
    MissingHost,
    HostNotCataloged { host: String },
    PortNotHttps,
    IpHost,
    EmptyPath,
    TooManyRedirects,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogUrl {
    pub https: Url,
}

pub fn validate_catalog_url(
    raw: &str,
    allowed_hosts: &[&str],
) -> Result<CatalogUrl, UrlPolicyError> {
    let url = Url::parse(raw).map_err(|_| UrlPolicyError::NotHttps)?;
    check_url(&url, allowed_hosts)?;
    Ok(CatalogUrl { https: url })
}

pub fn check_url(url: &Url, allowed_hosts: &[&str]) -> Result<(), UrlPolicyError> {
    if url.scheme() != "https" {
        return Err(UrlPolicyError::NotHttps);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UrlPolicyError::Userinfo);
    }
    match url.port() {
        None | Some(443) => {}
        Some(_) => return Err(UrlPolicyError::PortNotHttps),
    }
    let host = match url.host() {
        Some(url::Host::Domain(host)) => host,
        Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) => return Err(UrlPolicyError::IpHost),
        None => return Err(UrlPolicyError::MissingHost),
    };
    if host.is_empty() {
        return Err(UrlPolicyError::MissingHost);
    }
    let allowed = allowed_hosts
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(host));
    if !allowed {
        return Err(UrlPolicyError::HostNotCataloged {
            host: host.to_owned(),
        });
    }
    if url.path().is_empty() {
        return Err(UrlPolicyError::EmptyPath);
    }
    Ok(())
}

pub fn resolve_redirect(
    previous: &Url,
    location: &str,
    allowed_hosts: &[&str],
    hops: u8,
) -> Result<CatalogUrl, UrlPolicyError> {
    if hops >= MAX_REDIRECTS {
        return Err(UrlPolicyError::TooManyRedirects);
    }
    let joined = previous
        .join(location)
        .map_err(|_| UrlPolicyError::NotHttps)?;
    check_url(&joined, allowed_hosts)?;
    Ok(CatalogUrl { https: joined })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSTS: &[&str] = &["fixtures.voisu.test", "huggingface.co"];

    #[test]
    fn https_catalog_host_is_accepted() {
        validate_catalog_url("https://fixtures.voisu.test/l3/model.bin", HOSTS).unwrap();
    }

    #[test]
    fn http_userinfo_ip_and_unknown_host_are_rejected() {
        assert_eq!(
            validate_catalog_url("http://fixtures.voisu.test/x", HOSTS),
            Err(UrlPolicyError::NotHttps)
        );
        assert_eq!(
            validate_catalog_url("https://user:pass@fixtures.voisu.test/x", HOSTS),
            Err(UrlPolicyError::Userinfo)
        );
        assert_eq!(
            validate_catalog_url("https://127.0.0.1/x", HOSTS),
            Err(UrlPolicyError::IpHost)
        );
        assert!(matches!(
            validate_catalog_url("https://evil.example/x", HOSTS),
            Err(UrlPolicyError::HostNotCataloged { host }) if host == "evil.example"
        ));
        assert_eq!(
            validate_catalog_url("https://fixtures.voisu.test:8443/x", HOSTS),
            Err(UrlPolicyError::PortNotHttps)
        );
    }

    #[test]
    fn fourth_redirect_is_rejected() {
        let start = Url::parse("https://fixtures.voisu.test/a").unwrap();
        let error = resolve_redirect(&start, "/b", HOSTS, MAX_REDIRECTS).unwrap_err();
        assert_eq!(error, UrlPolicyError::TooManyRedirects);
    }

    #[test]
    fn redirect_escape_to_other_host_is_rejected() {
        let start = Url::parse("https://fixtures.voisu.test/a").unwrap();
        let error = resolve_redirect(&start, "https://evil.example/steal", HOSTS, 0).unwrap_err();
        assert!(matches!(error, UrlPolicyError::HostNotCataloged { .. }));
    }
}
