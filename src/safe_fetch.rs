//! Fetches pages that strangers point us at, without letting them reach internal addresses.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::Policy;
use url::Url;

const MAX_REDIRECTS: usize = 5;
const MAX_BYTES: usize = 3 * 1024 * 1024;
const MAX_PDF_BYTES: usize = 8 * 1024 * 1024;
const USER_AGENT: &str = "WalkInImporter/0.1";

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The address itself is not allowed. The message is safe to show.
    #[error("{0}")]
    Unsafe(String),
    #[error("could not read {url}: {reason}")]
    Failed { url: String, reason: String },
}

pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || a == 0
                || a >= 240
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && (b == 18 || b == 19))
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_ip(IpAddr::V4(mapped));
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
    }
}

/// Resolves at connection time, so a name that looked public during validation
/// cannot swap to a private address before the socket opens.
struct GuardedResolver;

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = format!("{}:0", name.as_str());
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host(host).await?.collect();
            if addresses.iter().any(|address| is_blocked_ip(address.ip())) {
                let error: Box<dyn std::error::Error + Send + Sync> =
                    "that address resolves to a private network".into();
                return Err(error);
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

pub struct FetchedPage {
    pub url: Url,
    pub body: String,
}

#[derive(Clone)]
pub struct SafeFetcher {
    client: reqwest::Client,
    allow_private: bool,
}

impl SafeFetcher {
    pub fn new(allow_private: bool) -> anyhow::Result<Self> {
        let mut builder = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(12))
            .user_agent(USER_AGENT);
        if !allow_private {
            builder = builder.dns_resolver(Arc::new(GuardedResolver));
        }
        Ok(Self { client: builder.build()?, allow_private })
    }

    pub fn check_url(&self, raw: &str) -> Result<Url, FetchError> {
        let unsafe_url = |message: &str| FetchError::Unsafe(message.to_owned());
        let url = Url::parse(raw).map_err(|_| unsafe_url("That does not look like a web address."))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(unsafe_url("Only http and https addresses can be read."));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(unsafe_url("Addresses with embedded logins cannot be read."));
        }
        if self.allow_private {
            return Ok(url);
        }
        if url.port().is_some_and(|port| port != 80 && port != 443) {
            return Err(unsafe_url("Only standard web ports can be read."));
        }
        let private = unsafe_url("Private addresses cannot be read.");
        match url.host() {
            None => return Err(private),
            Some(url::Host::Domain(domain)) => {
                let domain = domain.to_ascii_lowercase();
                if domain == "localhost" || domain.ends_with(".localhost") || domain.ends_with(".internal") {
                    return Err(private);
                }
            }
            Some(url::Host::Ipv4(ip)) if is_blocked_ip(IpAddr::V4(ip)) => return Err(private),
            Some(url::Host::Ipv6(ip)) if is_blocked_ip(IpAddr::V6(ip)) => return Err(private),
            Some(_) => {}
        }
        Ok(url)
    }

    /// Opens a URL, re-validating the target on every redirect hop.
    async fn open(&self, raw: &str, accept: &str) -> Result<(Url, reqwest::Response), FetchError> {
        let mut current = self.check_url(raw)?;
        for _ in 0..=MAX_REDIRECTS {
            let failed = |reason: String, url: &Url| FetchError::Failed { url: url.to_string(), reason };
            let response = self
                .client
                .get(current.clone())
                .header("accept", accept)
                .send()
                .await
                .map_err(|error| failed(error.to_string(), &current))?;
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| failed("redirect without a location".to_owned(), &current))?;
                let next = current.join(location).map_err(|error| failed(error.to_string(), &current))?;
                current = self.check_url(next.as_str())?;
                continue;
            }
            if !status.is_success() {
                return Err(failed(format!("HTTP {status}"), &current));
            }
            return Ok((current, response));
        }
        Err(FetchError::Failed { url: raw.to_owned(), reason: "too many redirects".to_owned() })
    }

    /// Fetches a text document.
    pub async fn fetch(&self, raw: &str) -> Result<FetchedPage, FetchError> {
        let (url, response) = self.open(raw, "text/html,text/plain;q=0.9").await?;
        let content_type = content_type(&response);
        if !content_type.is_empty() && !content_type.contains("text/") && !content_type.contains("xhtml") {
            return Err(FetchError::Failed { url: url.to_string(), reason: format!("not a web page ({content_type})") });
        }
        let bytes = read_limited(response, MAX_BYTES).await.map_err(|error| FetchError::Failed { url: url.to_string(), reason: error.to_string() })?;
        Ok(FetchedPage { url, body: String::from_utf8_lossy(&bytes).into_owned() })
    }

    /// Fetches a PDF. One over the limit is refused, because a cut-off file can't be parsed.
    pub async fn fetch_pdf(&self, raw: &str) -> Result<(Url, Vec<u8>), FetchError> {
        let (url, response) = self.open(raw, "application/pdf").await?;
        let failed = |reason: String, url: &Url| FetchError::Failed { url: url.to_string(), reason };
        let content_type = content_type(&response);
        if !content_type.is_empty() && !content_type.contains("pdf") && !content_type.contains("octet-stream") {
            return Err(failed(format!("not a PDF ({content_type})"), &url));
        }
        let bytes = read_limited(response, MAX_PDF_BYTES + 1).await.map_err(|error| failed(error.to_string(), &url))?;
        if bytes.len() > MAX_PDF_BYTES {
            return Err(failed("too large".to_owned(), &url));
        }
        Ok((url, bytes))
    }
}

fn content_type(response: &reqwest::Response) -> String {
    response.headers().get("content-type").and_then(|value| value.to_str().ok()).unwrap_or("").to_owned()
}

async fn read_limited(mut response: reqwest::Response, limit: usize) -> reqwest::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= limit {
            bytes.truncate(limit);
            break;
        }
    }
    Ok(bytes)
}

/// A small robots.txt reader covering the rules that apply to every crawler.
pub fn robots_disallows(robots: &str, path: &str) -> bool {
    let our_agent = USER_AGENT.split('/').next().unwrap_or_default().to_ascii_lowercase();
    let (mut in_group, mut reading_agents) = (false, false);
    let (mut disallowed, mut allowed) = (Vec::new(), Vec::new());
    for line in robots.lines() {
        let line = line.split('#').next().unwrap_or_default();
        let Some((key, value)) = line.split_once(':') else { continue };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        match key.as_str() {
            "user-agent" => {
                let matches = value == "*" || value.eq_ignore_ascii_case(&our_agent);
                in_group = if reading_agents { in_group || matches } else { matches };
                reading_agents = true;
            }
            "disallow" | "allow" => {
                reading_agents = false;
                if in_group && !value.is_empty() {
                    if key == "allow" { allowed.push(value) } else { disallowed.push(value) }
                }
            }
            _ => {}
        }
    }
    let longest = |rules: &[&str]| {
        rules.iter().filter(|rule| path.starts_with(**rule)).map(|rule| rule.len() as i64).max().unwrap_or(-1)
    };
    longest(&disallowed) > longest(&allowed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_internal_addresses() {
        for ip in ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fe80::1", "fc00::1", "::ffff:127.0.0.1", "::ffff:169.254.169.254"] {
            assert!(is_blocked_ip(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(!is_blocked_ip(ip.parse().unwrap()), "{ip} should be allowed");
        }
    }

    #[test]
    fn rejects_unsafe_urls() {
        let fetcher = SafeFetcher::new(false).unwrap();
        for url in ["http://localhost/", "http://127.0.0.1/", "http://[::1]/", "http://169.254.169.254/latest/", "ftp://example.com/", "file:///etc/passwd", "http://user:pass@example.com/", "http://example.com:8080/", "http://metadata.google.internal/"] {
            assert!(fetcher.check_url(url).is_err(), "{url} should be rejected");
        }
        assert!(fetcher.check_url("https://example.com/shop").is_ok());
    }

    #[tokio::test]
    async fn resolver_refuses_names_that_point_inward() {
        let fetcher = SafeFetcher::new(false).unwrap();
        // Passes URL validation, but resolves to loopback once the connection is attempted.
        assert!(fetcher.fetch("http://localtest.me/").await.is_err());
    }

    #[test]
    fn respects_robots_rules() {
        let robots = "User-agent: *\nDisallow: /private\nAllow: /private/menu\n\nUser-agent: Googlebot\nDisallow: /";
        assert!(robots_disallows(robots, "/private/x"));
        assert!(!robots_disallows(robots, "/private/menu"));
        assert!(!robots_disallows(robots, "/shop"));
        assert!(robots_disallows("User-agent: WalkInImporter\nDisallow: /", "/shop"));
    }
}
