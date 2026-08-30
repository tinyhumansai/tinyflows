//! Outbound HTTP for `http_request` nodes, behind a host allowlist.
//!
//! Two guards stand between a workflow author and the network, and they are
//! deliberately separate:
//!
//! 1. **The allowlist**, which is policy: a host an operator has agreed this
//!    workflow may reach. Empty by default, so a freshly installed workflow
//!    cannot become an exfiltration path.
//! 2. **The loopback and private-range refusal**, which is not policy: reaching
//!    `127.0.0.1` or `10.x` from a workflow means reaching services that trusted
//!    the network boundary, so it is refused whatever the allowlist says.
//!
//! Credentials never appear in the graph. A node names one with an opaque
//! `connection_ref` of the form `http_cred:<name>`, resolved here against the
//! host's store and injected into the request *after* any summary of the call
//! has been taken — so a secret cannot reach a log, an approval prompt, or a
//! node's recorded output.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;

use crate::caps::HttpClient;
use crate::error::{EngineError, Result};

/// The `connection_ref` prefix naming an HTTP credential.
pub const HTTP_CRED_PREFIX: &str = "http_cred:";

/// How long a workflow HTTP request waits for a TCP/TLS connection.
///
/// A black-holed peer must fail the node, not hang it forever.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long a workflow HTTP request tolerates a silent socket mid-response.
///
/// An idle timeout, not a whole-request one: a peer that keeps sending bytes
/// (a streamed response) must not be cut off schedule.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// How long `permit` waits for the DNS lookup inside [`vet_resolution`].
///
/// Separate from [`CONNECT_TIMEOUT`], which bounds only the TCP/TLS handshake
/// that follows a successful resolution — a resolver that never answers is
/// not covered by it at all.
const DNS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A credential the host injects into an outbound request.
#[derive(Debug, Clone)]
pub struct HttpCredential {
    /// The header to set, e.g. `Authorization`.
    pub header: String,
    /// The header's value, e.g. `Bearer …`. Never logged.
    pub value: String,
}

/// The credential name inside a `connection_ref`, if it names one.
///
/// Fails closed: a `connection_ref` that is present but not an HTTP credential
/// reference is an error rather than a silently unauthenticated request, because
/// silently dropping the credential would send the call anyway.
pub fn http_cred_name(conn: Option<&str>) -> Result<Option<&str>> {
    let Some(conn) = conn.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    conn.strip_prefix(HTTP_CRED_PREFIX)
        .map(Some)
        .filter(|name| name.is_some_and(|n| !n.is_empty()))
        .ok_or_else(|| {
            EngineError::Capability(format!(
                "http_request: unrecognised connection_ref '{conn}'; expected \
                 '{HTTP_CRED_PREFIX}<name>'"
            ))
        })
}

/// Merge `cred` into `request`'s headers, returning the request to send.
///
/// Called last, after the request has been described for logs or approval, so
/// the secret exists only in the value handed to the transport.
pub fn inject_credential(mut request: Value, cred: &HttpCredential) -> Value {
    if let Some(object) = request.as_object_mut() {
        let headers = object
            .entry("headers")
            .or_insert_with(|| Value::Object(Default::default()));
        if let Some(headers) = headers.as_object_mut() {
            headers.insert(cred.header.clone(), Value::String(cred.value.clone()));
        }
    }
    request
}

/// A description of a request safe to log or show for approval: method and URL
/// only, never headers or body.
pub fn redacted_summary(request: &Value) -> String {
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let url = request.get("url").and_then(Value::as_str).unwrap_or("");
    format!("{method} {url}")
}

/// The hosts an `http_request` node may reach.
///
/// Policy, and therefore the host's to state — but the *shape* of the policy is
/// the same wherever this client runs, so the matching rule lives here rather
/// than being re-derived by each embedding application. Empty denies everything,
/// which is what makes a freshly installed workflow unable to become an
/// exfiltration path.
///
/// An entry matches its own name and any subdomain of it: `example.com` permits
/// `example.com` and `api.example.com`, but not `notexample.com`.
#[derive(Debug, Clone, Default)]
pub struct HostAllowlist {
    /// The permitted host suffixes, as the operator wrote them.
    entries: Vec<String>,
}

impl HostAllowlist {
    /// An allowlist over `entries`.
    pub fn new<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            entries: entries.into_iter().map(Into::into).collect(),
        }
    }

    /// Whether `host` is permitted, by exact match or as a subdomain.
    ///
    /// Case- and whitespace-insensitive on both sides, because an operator's
    /// configuration and a URL's authority are written by different people.
    #[must_use]
    pub fn allows(&self, host: &str) -> bool {
        let host = host.trim().to_ascii_lowercase();
        if host.is_empty() {
            return false;
        }
        self.entries.iter().any(|allowed| {
            let allowed = allowed.trim().to_ascii_lowercase();
            if allowed.is_empty() {
                return false;
            }
            host == allowed || host.ends_with(&format!(".{allowed}"))
        })
    }

    /// Whether the list permits nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(|entry| entry.trim().is_empty())
    }
}

/// An [`HttpClient`] over `reqwest`, gated by the host's allowlist.
pub struct AllowlistHttpClient {
    allowlist: HostAllowlist,
    credentials: HashMap<String, HttpCredential>,
    client: reqwest::Client,
}

impl AllowlistHttpClient {
    /// A client permitting only what `allowlist` allows, resolving
    /// `connection_ref`s against `credentials`.
    ///
    /// # Panics
    /// Panics if the underlying `reqwest` client cannot be built. A silent
    /// fallback to `reqwest::Client::default()` would drop both the redirect
    /// refusal and the timeouts configured below, producing a client that
    /// looks the same but no longer enforces either guard — worse than
    /// failing loudly at construction, which happens once at startup.
    pub fn new(allowlist: HostAllowlist, credentials: HashMap<String, HttpCredential>) -> Self {
        Self {
            allowlist,
            credentials,
            // Redirects are refused rather than followed. A permitted host that
            // 302s to `169.254.169.254` or to an unlisted domain would
            // otherwise walk straight past both guards, since only the first
            // URL is ever checked. A workflow that genuinely needs to follow one
            // can make the second request itself, where it is checked again.
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .build()
                .expect("reqwest client with static config must build"),
        }
    }

    /// Check the URL against both guards, returning the parsed URL and the
    /// vetted addresses the request may connect to.
    ///
    /// The address list is returned rather than discarded because vetting a
    /// name and then letting the transport resolve it a second time is a
    /// rebinding window: a short-TTL answer can be private by the time the
    /// connection is made. The caller pins the transport to exactly these.
    async fn permit(&self, request: &Value) -> Result<(reqwest::Url, Vec<std::net::SocketAddr>)> {
        let raw = request
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| EngineError::Capability("http_request: no url".to_string()))?;
        let url = reqwest::Url::parse(raw)
            .map_err(|err| EngineError::Capability(format!("http_request: invalid url: {err}")))?;

        if !matches!(url.scheme(), "http" | "https") {
            return Err(EngineError::Capability(format!(
                "http_request: refusing scheme '{}'",
                url.scheme()
            )));
        }
        let host = url
            .host_str()
            .ok_or_else(|| EngineError::Capability("http_request: url has no host".to_string()))?;
        if is_private_host(host) {
            return Err(EngineError::Capability(format!(
                "http_request: refusing '{host}': loopback and private addresses are not \
                 reachable from a workflow"
            )));
        }
        if !self.allowlist.allows(host) {
            return Err(EngineError::Capability(format!(
                "http_request: '{host}' is not in the configured http allowlist"
            )));
        }
        // Last, because it is the only check that touches the network: an
        // allowlisted name must not resolve into a range the guard above
        // refuses by literal.
        let port = url
            .port_or_known_default()
            .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
        let vetted = resolve_with_timeout(host.to_string(), port).await?;
        Ok((url, vetted))
    }

    /// A client that can only connect to `addrs` when it resolves `host`.
    ///
    /// Built per request rather than shared, because the override is a builder
    /// option and the host is not known until one arrives. That costs a client
    /// construction per call and gives up connection pooling; the alternative
    /// is letting the transport perform its own lookup, which is precisely the
    /// second resolution this exists to remove.
    ///
    /// An IP-literal host needs no override — there is no name to resolve, and
    /// the literal has already been judged by `is_private_host`.
    fn pinned(&self, host: &str, addrs: &[std::net::SocketAddr]) -> Result<reqwest::Client> {
        let bare = host.trim_matches(['[', ']']);
        if addrs.is_empty() || bare.parse::<std::net::IpAddr>().is_ok() {
            return Ok(self.client.clone());
        }
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(bare, addrs)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|err| {
                EngineError::Capability(format!("http_request: cannot build client: {err}"))
            })
    }
}

/// Whether an address is one a workflow must never reach.
///
/// Loopback, link-local (which includes the cloud metadata endpoint at
/// `169.254.169.254`), and the RFC 1918 ranges. Reaching any of them from a
/// workflow means reaching services that trusted the network boundary.
pub fn is_private_addr(addr: &std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => is_private_v4(v4),
        std::net::IpAddr::V6(v6) => {
            // An IPv4-mapped address is an IPv4 address wearing a hat:
            // `::ffff:127.0.0.1` reaches loopback just as `127.0.0.1` does, so
            // it must be judged by the same rules rather than falling through
            // the v6 checks below.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_private_v4(&mapped);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                // link-local fe80::/10
                || v6.segments()[0] & 0xffc0 == 0xfe80
                // unique-local fc00::/7 — the v6 answer to RFC 1918
                || v6.segments()[0] & 0xfe00 == 0xfc00
        }
    }
}

/// The IPv4 ranges a workflow must never reach.
fn is_private_v4(addr: &std::net::Ipv4Addr) -> bool {
    // `is_shared` (`100.64.0.0/10`, RFC 6598 carrier-grade NAT) is not yet a
    // stable `std` method, so the range is checked by hand. It fronts internal
    // infrastructure on many cloud and carrier networks the same way RFC 1918
    // does, and an allowlisted name resolving into it must be refused for the
    // same reason.
    let shared = addr.octets()[0] == 100 && (addr.octets()[1] & 0xC0) == 0x40;
    addr.is_loopback()
        || addr.is_private()
        || addr.is_link_local()
        || addr.is_unspecified()
        || shared
}

/// Runs [`vet_resolution`] on a blocking-pool thread with a bounded wait.
///
/// `permit` runs on the request path, and the synchronous
/// `ToSocketAddrs::to_socket_addrs` inside `vet_resolution` performs a real DNS
/// lookup — potentially a slow one, against a resolver this process does not
/// control. Run it off the async worker so a slow resolver cannot stall other
/// tasks sharing that thread; `CONNECT_TIMEOUT` only bounds the TCP handshake
/// that follows a successful resolution, not the lookup before it, so this
/// needs its own timeout.
///
/// Timing out the join handle does not cancel an already-running blocking
/// call: `to_socket_addrs` keeps running to completion on the blocking pool
/// and its result is simply discarded once this returns. Tokio's blocking
/// pool has its own bound on concurrent threads, which caps how many stalled
/// lookups can accumulate; a resolver that hangs on every call still degrades
/// throughput, just not into an unbounded thread leak.
///
/// `vet_resolution` itself stays synchronous — moving only the call site keeps
/// the existing unit tests, which call it directly, working unchanged.
async fn resolve_with_timeout(host: String, port: u16) -> Result<Vec<std::net::SocketAddr>> {
    let lookup = tokio::task::spawn_blocking(move || vet_resolution(&host, port));
    match tokio::time::timeout(DNS_TIMEOUT, lookup).await {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(_join_error)) => Err(EngineError::Capability(
            "http_request: dns lookup task panicked".to_string(),
        )),
        Err(_elapsed) => Err(EngineError::Capability(
            "http_request: dns lookup timed out".to_string(),
        )),
    }
}

/// Every address `host` resolves to, refused if any is private.
///
/// The textual check alone is not enough: an allowlisted name whose DNS answer
/// is `127.0.0.1` would otherwise pass both guards. Resolving makes the guard
/// depend on the network it is guarding, which is the trade — but the failure
/// mode of not resolving is an authored workflow reaching internal services,
/// and that is worse than a lookup.
///
/// The vetted addresses are *returned*, not just judged: the caller pins the
/// transport to them, so the answer checked here is the answer connected to. A
/// second, independent lookup by the transport would reopen the rebinding gap
/// this closes — a name whose record flips to `169.254.169.254` between the two
/// resolutions passes the guard and reaches metadata anyway.
///
/// A name that cannot be resolved at all is refused rather than allowed: the
/// request would fail anyway, and failing here says why. So is one that
/// resolves to nothing, which would otherwise pin the transport to an empty
/// set and let it fall back to its own lookup.
pub fn vet_resolution(host: &str, port: u16) -> Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;

    let resolved: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|err| {
            EngineError::Capability(format!("http_request: cannot resolve '{host}': {err}"))
        })?
        .collect();

    if let Some(private) = resolved.iter().find(|addr| is_private_addr(&addr.ip())) {
        return Err(EngineError::Capability(format!(
            "http_request: refusing '{host}': it resolves to {}, which is loopback or private",
            private.ip()
        )));
    }
    if resolved.is_empty() {
        return Err(EngineError::Capability(format!(
            "http_request: cannot resolve '{host}': it has no addresses"
        )));
    }
    Ok(resolved)
}

/// Whether a host *names* loopback, a link-local address, or an RFC 1918 range.
///
/// The cheap textual guard, applied before any lookup. The authoritative check
/// is `vet_resolution`, which catches the names this cannot.
pub fn is_private_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".internal") {
        return true;
    }
    if let Ok(addr) = host.parse::<std::net::IpAddr>() {
        return is_private_addr(&addr);
    }
    false
}

#[async_trait]
impl HttpClient for AllowlistHttpClient {
    async fn request(&self, request: Value, conn: Option<&str>) -> Result<Value> {
        let (url, vetted) = self.permit(&request).await?;
        let summary = redacted_summary(&request);
        // Pinned to the addresses just vetted, so the connection cannot go
        // anywhere a second DNS answer might point.
        let host = url.host_str().unwrap_or_default().to_string();
        let client = self.pinned(&host, &vetted)?;

        // Resolve the credential before building the request, so an unknown name
        // fails before anything leaves the process.
        let credential = match http_cred_name(conn)? {
            Some(name) => Some(self.credentials.get(name).cloned().ok_or_else(|| {
                EngineError::Capability(format!("http_request: unknown credential '{name}'"))
            })?),
            None => None,
        };
        // `permit` allows both `http` and `https` — plain `http` is a
        // legitimate destination on its own. It stops being one the moment a
        // credential is going to ride along: `http` is cleartext, so a
        // resolved credential would be sent unencrypted. Refuse before
        // `inject_credential` rather than after, so the secret never touches
        // the outgoing request.
        if credential.is_some() && url.scheme() == "http" {
            return Err(EngineError::Capability(
                "http_request: refusing to send a credential over plain http".to_string(),
            ));
        }
        let request = match &credential {
            Some(cred) => inject_credential(request, cred),
            None => request,
        };

        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|err| EngineError::Capability(format!("http_request: {err}")))?;

        let mut builder = client.request(method, url);
        if let Some(headers) = request.get("headers").and_then(Value::as_object) {
            for (name, value) in headers {
                if let Some(value) = value.as_str() {
                    builder = builder.header(name, value);
                }
            }
        }
        if let Some(body) = request.get("body") {
            builder = builder.json(body);
        }

        let response = builder
            .send()
            .await
            .map_err(|err| EngineError::Capability(format!("http_request: {summary}: {err}")))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|err| EngineError::Capability(format!("http_request: {summary}: {err}")))?;
        let json: Option<Value> = serde_json::from_str(&text).ok();

        Ok(serde_json::json!({
            "status": status,
            "text": text,
            "json": json,
        }))
    }
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
