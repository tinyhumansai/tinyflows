//! Tests for the allowlisted HTTP client: the two guards, and credentials.
//!
//! The guards are the reason this implementation is shared rather than rewritten
//! per host. Each case here is one way an `http_request` node could otherwise
//! reach something it must not: a private literal, a public name whose DNS
//! answer is private, an IPv4 address wearing an IPv6 hat, a `connection_ref`
//! that names nothing.

use std::collections::HashMap;

use serde_json::json;

use super::*;
use crate::caps::HttpClient;

/// A client permitting `hosts` and holding no credentials.
fn client(hosts: &[&str]) -> AllowlistHttpClient {
    AllowlistHttpClient::new(HostAllowlist::new(hosts.to_vec()), HashMap::new())
}

#[tokio::test]
async fn http_refuses_loopback_and_anything_off_the_allowlist() {
    let client = client(&["example.com"]);

    // Loopback is refused even though the test could otherwise serve it — a
    // workflow reaching localhost is reaching services that trusted the network
    // boundary.
    let loopback = client
        .request(json!({ "url": "http://127.0.0.1:8080/x" }), None)
        .await
        .expect_err("loopback");
    assert!(loopback.to_string().contains("private"), "got {loopback}");

    let off_list = client
        .request(json!({ "url": "https://elsewhere.test/x" }), None)
        .await
        .expect_err("not allowlisted");
    assert!(off_list.to_string().contains("allowlist"), "got {off_list}");
}

#[test]
fn an_empty_allowlist_permits_nothing() {
    // The default, and the reason a freshly installed workflow cannot become an
    // exfiltration path without an operator saying so first.
    let list = HostAllowlist::default();

    assert!(list.is_empty());
    assert!(!list.allows("example.com"));
}

#[test]
fn an_allowlist_entry_covers_its_subdomains_but_not_its_lookalikes() {
    let list = HostAllowlist::new(["Example.com "]);

    assert!(list.allows("example.com"));
    assert!(list.allows("api.example.com"));
    assert!(!list.allows("notexample.com"));
    assert!(!list.allows(""));
}

#[test]
fn private_host_detection_covers_loopback_names_and_ranges_but_not_lookalikes() {
    for private in [
        "localhost",
        "127.0.0.1",
        "10.1.2.3",
        "192.168.0.1",
        "169.254.1.1",
        "::1",
        "db.internal",
    ] {
        assert!(is_private_host(private), "{private} should be refused");
    }
    for public in ["example.com", "notlocalhost.com", "8.8.8.8"] {
        assert!(!is_private_host(public), "{public} should be reachable");
    }
}

#[test]
fn an_unrecognised_connection_ref_fails_closed_rather_than_sending_unauthenticated() {
    assert_eq!(http_cred_name(None).unwrap(), None);
    assert_eq!(http_cred_name(Some("http_cred:ci")).unwrap(), Some("ci"));

    // Silently dropping it would send the request anyway, without the
    // credential the author asked for.
    assert!(http_cred_name(Some("composio:abc")).is_err());
    assert!(http_cred_name(Some("http_cred:")).is_err());
}

#[test]
fn a_credential_is_injected_after_the_summary_is_taken() {
    let request = json!({ "method": "post", "url": "https://example.com/x" });
    let summary = redacted_summary(&request);
    let sent = inject_credential(
        request,
        &HttpCredential {
            header: "Authorization".into(),
            value: "Bearer super-secret".into(),
        },
    );

    assert_eq!(summary, "POST https://example.com/x");
    assert!(
        !summary.contains("super-secret"),
        "a secret must never reach a log or an approval prompt"
    );
    assert_eq!(sent["headers"]["Authorization"], "Bearer super-secret");
}

#[test]
fn private_address_detection_covers_the_cloud_metadata_endpoint() {
    // The one an SSRF is usually aiming for, and the reason link-local is
    // refused rather than only loopback.
    let metadata: std::net::IpAddr = "169.254.169.254".parse().unwrap();
    assert!(is_private_addr(&metadata));

    for private in ["127.0.0.1", "10.0.0.1", "192.168.1.1", "172.16.0.1", "::1"] {
        let addr: std::net::IpAddr = private.parse().unwrap();
        assert!(is_private_addr(&addr), "{private}");
    }
    for public in ["8.8.8.8", "1.1.1.1"] {
        let addr: std::net::IpAddr = public.parse().unwrap();
        assert!(!is_private_addr(&addr), "{public}");
    }
}

#[tokio::test]
async fn an_allowlisted_name_that_resolves_to_loopback_is_still_refused() {
    // The textual guard cannot catch this: `localtest.me` and friends are
    // ordinary names whose DNS answer is 127.0.0.1. Resolving is what closes
    // the rebinding gap.
    let result = client(&["localtest.me"])
        .request(json!({ "url": "http://localtest.me/x" }), None)
        .await;

    // Either it resolved to loopback and was refused for that, or this machine
    // has no DNS for the name and it was refused for that — never sent.
    let err = result.expect_err("must not be sent");
    let message = err.to_string();
    assert!(
        message.contains("loopback or private") || message.contains("cannot resolve"),
        "got {message}"
    );
}

#[test]
fn vetting_returns_the_very_addresses_the_request_will_be_pinned_to() {
    // The vetted list is the point: it is handed to the transport as a DNS
    // override, so the answer checked here is the answer connected to and a
    // second lookup cannot rebind the name to something private in between.
    let refused = vet_resolution("localhost", 80).expect_err("loopback must not be vetted");
    assert!(
        refused.to_string().contains("loopback or private"),
        "{refused}"
    );

    // An IP literal resolves to itself, so a public one vets to exactly one
    // address and pins the transport to it.
    let vetted = vet_resolution("93.184.216.34", 443).expect("a public literal");
    assert_eq!(
        vetted,
        vec!["93.184.216.34:443".parse::<std::net::SocketAddr>().unwrap()]
    );
}

#[test]
fn an_ipv4_mapped_ipv6_loopback_is_recognised_as_private() {
    // `::ffff:127.0.0.1` reaches loopback exactly as `127.0.0.1` does, so
    // judging it by the v6 rules alone would let it through.
    for mapped in [
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::ffff:169.254.169.254",
    ] {
        let addr: std::net::IpAddr = mapped.parse().unwrap();
        assert!(is_private_addr(&addr), "{mapped}");
    }
    // Unique-local fc00::/7 — the v6 answer to RFC 1918.
    let ula: std::net::IpAddr = "fd00::1".parse().unwrap();
    assert!(is_private_addr(&ula));

    let public: std::net::IpAddr = "::ffff:8.8.8.8".parse().unwrap();
    assert!(!is_private_addr(&public));
}
