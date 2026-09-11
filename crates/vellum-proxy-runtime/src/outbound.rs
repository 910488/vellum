//! Outbound network-safety policy for third-party provider connections
//! (plan Stage D / V-outbound).
//!
//! `inbound.rs` protects the local proxy boundary from callers reaching in.
//! This module protects the credential the proxy holds from leaving in
//! plaintext toward the wrong place: a route's `base_url` is an arbitrary
//! address the user (or a hand-edited config file) supplied, and nothing
//! upstream of this module knows whether that address is loopback, a LAN
//! box, or the open internet. HTTPS is always fine. Plaintext HTTP is only
//! ever fine when the destination cannot expose a real secret in transit —
//! loopback always, private/LAN ranges only with explicit opt-in, and the
//! public internet only when the route carries no credential at all.
//!
//! DNS is resolved fresh at every call site that uses this module, never
//! cached across a "checked once, trusted forever" boundary: a hostname that
//! looked private when a route was saved can be repointed at a public
//! address before the next connection, and a one-A-record RFC1918 answer
//! must not launder a second, real-internet answer through the exemption.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

/// How a route's owner has opted into plaintext HTTP beyond the loopback
/// case, which is always allowed regardless of this setting.
///
/// `Deny` is the default: a freshly created or freshly loaded route that
/// never set this explicitly gets the safest behavior, not the most
/// permissive one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InsecureHttpPolicy {
    /// Plaintext HTTP is allowed only to loopback. Everything else over HTTP
    /// is refused.
    #[default]
    Deny,
    /// Plaintext HTTP to a private/LAN/CGNAT/link-local/ULA address is
    /// allowed in addition to loopback. Still never to a public address.
    AllowPrivateNetwork,
    /// Plaintext HTTP to a public address is allowed, but only when the
    /// route carries no credential — a real secret must never ride
    /// plaintext HTTP to the open internet, and no policy overrides that.
    AllowPublicWithoutCredentials,
}

/// Where a resolved address sits, from the perspective of "can plaintext
/// HTTP safely reach it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressClass {
    Loopback,
    Private,
    Public,
}

/// Classify one resolved address. IPv4-mapped IPv6 addresses are unwrapped
/// to their embedded IPv4 form first so `::ffff:8.8.8.8` classifies exactly
/// like `8.8.8.8`, not like an opaque IPv6 public address that happens to
/// dodge the IPv4 checks.
pub fn classify_ip(ip: IpAddr) -> AddressClass {
    match ip {
        IpAddr::V4(v4) => classify_ipv4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(mapped) => classify_ipv4(mapped),
            None => classify_ipv6(v6),
        },
    }
}

fn classify_ipv4(ip: Ipv4Addr) -> AddressClass {
    if ip.is_loopback() {
        return AddressClass::Loopback;
    }
    // RFC1918 (10/8, 172.16/12, 192.168/16) and 169.254/16 link-local.
    if ip.is_private() || ip.is_link_local() {
        return AddressClass::Private;
    }
    // CGNAT, RFC 6598: 100.64.0.0/10.
    let octets = ip.octets();
    if octets[0] == 100 && (octets[1] & 0b1100_0000) == 0b0100_0000 {
        return AddressClass::Private;
    }
    AddressClass::Public
}

fn classify_ipv6(ip: Ipv6Addr) -> AddressClass {
    if ip.is_loopback() {
        return AddressClass::Loopback;
    }
    // fe80::/10 link-local.
    if ip.is_unicast_link_local() {
        return AddressClass::Private;
    }
    // fc00::/7 unique local (ULA).
    let segments = ip.segments();
    if (segments[0] & 0xfe00) == 0xfc00 {
        return AddressClass::Private;
    }
    AddressClass::Public
}

/// The persistent Desktop-UI warning an allowed-but-insecure connection
/// requires. Two distinct variants because the private-network exemption and
/// the public-without-credentials exemption are not the same risk: the
/// latter puts every byte of the conversation on the open internet in the
/// clear, not just on a LAN segment the attacker must already be on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundWarning {
    PrivateNetworkHttp { host: String },
    PublicHttpWithoutCredentials { host: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OutboundWarningSeverity {
    Notice,
    Severe,
}

impl OutboundWarning {
    pub fn severity(&self) -> OutboundWarningSeverity {
        match self {
            OutboundWarning::PrivateNetworkHttp { .. } => OutboundWarningSeverity::Notice,
            OutboundWarning::PublicHttpWithoutCredentials { .. } => OutboundWarningSeverity::Severe,
        }
    }

    /// An i18n-key-shaped code, so a caller can build a `RuntimeNotice` (or
    /// equivalent) without re-deriving the distinction from prose.
    pub fn code(&self) -> &'static str {
        match self {
            OutboundWarning::PrivateNetworkHttp { .. } => "insecureHttpPrivateNetworkAllowed",
            OutboundWarning::PublicHttpWithoutCredentials { .. } => {
                "insecureHttpPublicWithoutCredentialsAllowed"
            }
        }
    }

    pub fn host(&self) -> &str {
        match self {
            OutboundWarning::PrivateNetworkHttp { host } => host,
            OutboundWarning::PublicHttpWithoutCredentials { host } => host,
        }
    }
}

impl fmt::Display for OutboundWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutboundWarning::PrivateNetworkHttp { host } => write!(
                f,
                "plaintext HTTP to private-network host `{host}` is allowed by this route's policy"
            ),
            OutboundWarning::PublicHttpWithoutCredentials { host } => write!(
                f,
                "plaintext HTTP to public host `{host}` is allowed by this route's policy \
                 (no credential is configured, but every byte of the exchange is unencrypted)"
            ),
        }
    }
}

/// Why an outbound connection was refused. Each case is distinct on purpose:
/// the Desktop UI and the runtime's own error mapping need to tell a user
/// "turn on AllowPrivateNetwork" apart from "this route has a credential, no
/// setting will let plaintext HTTP reach the public internet".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutboundPolicyError {
    #[error(
        "plaintext HTTP to private-network host `{host}` requires this route's \
         insecureHttpPolicy to be set to allowPrivateNetwork"
    )]
    PrivateNetworkRequiresPolicy { host: String },
    #[error(
        "plaintext HTTP to public host `{host}` is refused: this route carries a credential, \
         and no policy setting may send a credential over plaintext HTTP to the public internet"
    )]
    PublicHttpWithCredentialsDenied { host: String },
    #[error(
        "plaintext HTTP to public host `{host}` requires this route's insecureHttpPolicy to be \
         set to allowPublicWithoutCredentials"
    )]
    PublicHttpRequiresPolicy { host: String },
    #[error("could not resolve host `{host}`: {reason}")]
    DnsResolutionFailed { host: String, reason: String },
    #[error("invalid upstream URL `{url}`: {reason}")]
    InvalidUrl { url: String, reason: String },
}

/// The pure decision core: given already-resolved addresses for a host, the
/// scheme in use, whether the route carries a credential, and its policy,
/// decide whether the connection may proceed.
///
/// Deliberately synchronous and allocation-light so it can be unit tested
/// exhaustively without a network or an async runtime, and so both the sync
/// (Desktop admission) and async (transport pre-connect) callers share
/// exactly one rulebook.
///
/// If *any* resolved address for the host is public, the whole host is
/// treated as public — a hostname with one RFC1918 answer and one real
/// public answer must not borrow the private-network exemption.
pub fn decide(
    host: &str,
    resolved_ips: &[IpAddr],
    is_https: bool,
    has_credentials: bool,
    policy: InsecureHttpPolicy,
) -> Result<Option<OutboundWarning>, OutboundPolicyError> {
    if is_https {
        return Ok(None);
    }
    if resolved_ips.is_empty() {
        return Err(OutboundPolicyError::DnsResolutionFailed {
            host: host.to_string(),
            reason: "no addresses resolved".into(),
        });
    }
    let classes: Vec<AddressClass> = resolved_ips.iter().copied().map(classify_ip).collect();
    if classes.contains(&AddressClass::Public) {
        if has_credentials {
            return Err(OutboundPolicyError::PublicHttpWithCredentialsDenied {
                host: host.to_string(),
            });
        }
        return match policy {
            InsecureHttpPolicy::AllowPublicWithoutCredentials => {
                Ok(Some(OutboundWarning::PublicHttpWithoutCredentials {
                    host: host.to_string(),
                }))
            }
            InsecureHttpPolicy::Deny | InsecureHttpPolicy::AllowPrivateNetwork => {
                Err(OutboundPolicyError::PublicHttpRequiresPolicy {
                    host: host.to_string(),
                })
            }
        };
    }
    if classes.iter().all(|class| *class == AddressClass::Loopback) {
        return Ok(None);
    }
    // At least one private/LAN address and nothing public.
    match policy {
        InsecureHttpPolicy::AllowPrivateNetwork => Ok(Some(OutboundWarning::PrivateNetworkHttp {
            host: host.to_string(),
        })),
        InsecureHttpPolicy::Deny | InsecureHttpPolicy::AllowPublicWithoutCredentials => {
            Err(OutboundPolicyError::PrivateNetworkRequiresPolicy {
                host: host.to_string(),
            })
        }
    }
}

/// Resolve a host to its addresses, synchronously. Used for admission-time
/// checks (Desktop route create/edit, which run outside an async context)
/// and is safe to call again immediately before connecting — this function
/// never caches.
///
/// An IP literal is returned without a DNS round-trip; a bracketed IPv6
/// literal (`[::1]`) is unwrapped first.
pub fn resolve_host_sync(host: &str) -> Result<Vec<IpAddr>, String> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    (bare, 0u16)
        .to_socket_addrs()
        .map(|addrs| addrs.map(|addr| addr.ip()).collect())
        .map_err(|error| error.to_string())
}

/// The async counterpart to [`resolve_host_sync`], for the transport's
/// pre-connect check. Kept as a real async DNS lookup (not
/// `spawn_blocking` over the sync resolver) so it does not borrow a blocking
/// thread on every third-party request.
pub async fn resolve_host_async(host: &str) -> Result<Vec<IpAddr>, String> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    tokio::net::lookup_host((bare, 0u16))
        .await
        .map(|addrs| addrs.map(|addr| addr.ip()).collect())
        .map_err(|error| error.to_string())
}

/// Parse `url_str`, extract its scheme and host, and decide against a fresh
/// synchronous DNS resolution. This is the whole admission-time check.
pub fn validate_outbound_url_sync(
    url_str: &str,
    has_credentials: bool,
    policy: InsecureHttpPolicy,
) -> Result<Option<OutboundWarning>, OutboundPolicyError> {
    let (is_https, host) = parse_scheme_and_host(url_str)?;
    if is_https {
        return Ok(None);
    }
    let ips =
        resolve_host_sync(&host).map_err(|reason| OutboundPolicyError::DnsResolutionFailed {
            host: host.clone(),
            reason,
        })?;
    decide(&host, &ips, is_https, has_credentials, policy)
}

/// The async counterpart to [`validate_outbound_url_sync`], for the
/// transport's pre-connect check and for re-validating each redirect hop.
pub async fn validate_outbound_url_async(
    url_str: &str,
    has_credentials: bool,
    policy: InsecureHttpPolicy,
) -> Result<Option<OutboundWarning>, OutboundPolicyError> {
    let (is_https, host) = parse_scheme_and_host(url_str)?;
    if is_https {
        return Ok(None);
    }
    let ips = resolve_host_async(&host).await.map_err(|reason| {
        OutboundPolicyError::DnsResolutionFailed {
            host: host.clone(),
            reason,
        }
    })?;
    decide(&host, &ips, is_https, has_credentials, policy)
}

fn parse_scheme_and_host(url_str: &str) -> Result<(bool, String), OutboundPolicyError> {
    let url = url::Url::parse(url_str).map_err(|error| OutboundPolicyError::InvalidUrl {
        url: url_str.to_string(),
        reason: error.to_string(),
    })?;
    let is_https = url.scheme().eq_ignore_ascii_case("https");
    if !is_https && !url.scheme().eq_ignore_ascii_case("http") {
        return Err(OutboundPolicyError::InvalidUrl {
            url: url_str.to_string(),
            reason: format!(
                "unsupported scheme `{}`; only http/https are routable",
                url.scheme()
            ),
        });
    }
    let host = url
        .host_str()
        .ok_or_else(|| OutboundPolicyError::InvalidUrl {
            url: url_str.to_string(),
            reason: "URL has no host".into(),
        })?
        .to_string();
    Ok((is_https, host))
}

/// Bundles the two pieces of route context the transport needs to enforce
/// this policy per-request, since [`crate::transport::UpstreamTransport`]
/// itself stays dialect- and policy-agnostic. Carried on
/// [`crate::transport::UpstreamRequest`]; `None` there means the caller is
/// exempt (the Official route's fixed HTTPS endpoint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboundRequestPolicy {
    pub insecure_http: InsecureHttpPolicy,
    pub has_credentials: bool,
}

/// Single third-party (never Official) pre-dispatch check.
///
/// Official OpenAI Responses is a native passthrough and must not call this
/// function. If it is invoked for an Official route anyway, the helper is a
/// no-op so a mistaken call cannot rewrite Official HTTPS into this policy.
pub fn check_third_party_url(
    route: &crate::route::RuntimeModelRoute,
    url: &str,
    has_credential: bool,
) -> Result<Option<OutboundWarning>, OutboundPolicyError> {
    if route.provider_kind == crate::route::RuntimeProviderKind::Official {
        return Ok(None);
    }
    validate_outbound_url_sync(url, has_credential, route.insecure_http_policy)
}

/// Whether a stored third-party route would fail admission and should stay
/// on disk but be marked disabled until the owner opts in.
///
/// Official routes are never gated. DNS failures are not admission refusals:
/// an offline laptop must not disable a previously-valid hostname.
pub fn stored_route_fails_admission(
    provider_is_official: bool,
    url: &str,
    has_credential: bool,
    policy: InsecureHttpPolicy,
) -> bool {
    if provider_is_official {
        return false;
    }
    match validate_outbound_url_sync(url, has_credential, policy) {
        Ok(_) => false,
        Err(OutboundPolicyError::DnsResolutionFailed { .. }) => false,
        Err(_) => true,
    }
}

/// Whether a route is configured to send a secret (API key / OAuth / session).
pub fn route_carries_credential(route: &crate::route::RuntimeModelRoute) -> bool {
    route.credential_id.is_some() || !matches!(route.auth_kind, crate::route::RuntimeAuthKind::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIVATE_V4: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3));
    const CGNAT_V4: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
    const LINK_LOCAL_V4: IpAddr = IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1));
    const PUBLIC_V4: IpAddr = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
    const LOOPBACK_V4: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    const LOOPBACK_V6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);
    const ULA_V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1));
    const LINK_LOCAL_V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));
    const PUBLIC_V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888));

    // ---- address classification ----

    #[test]
    fn classifies_loopback_addresses() {
        assert_eq!(classify_ip(LOOPBACK_V4), AddressClass::Loopback);
        assert_eq!(classify_ip(LOOPBACK_V6), AddressClass::Loopback);
    }

    #[test]
    fn classifies_private_lan_cgnat_link_local_and_ula_as_private() {
        for ip in [PRIVATE_V4, CGNAT_V4, LINK_LOCAL_V4, ULA_V6, LINK_LOCAL_V6] {
            assert_eq!(
                classify_ip(ip),
                AddressClass::Private,
                "{ip} must be private"
            );
        }
        assert_eq!(
            classify_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 5))),
            AddressClass::Private
        );
        assert_eq!(
            classify_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5))),
            AddressClass::Private
        );
        // 172.32/12 is outside the RFC1918 172.16/12 block and must not be
        // misclassified as private.
        assert_eq!(
            classify_ip(IpAddr::V4(Ipv4Addr::new(172, 32, 0, 5))),
            AddressClass::Public
        );
    }

    #[test]
    fn classifies_real_internet_addresses_as_public() {
        assert_eq!(classify_ip(PUBLIC_V4), AddressClass::Public);
        assert_eq!(classify_ip(PUBLIC_V6), AddressClass::Public);
    }

    #[test]
    fn an_ipv4_mapped_ipv6_address_classifies_like_its_embedded_ipv4() {
        let mapped_private = IpAddr::V6(Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped());
        assert_eq!(classify_ip(mapped_private), AddressClass::Private);
        let mapped_public = IpAddr::V6(
            PUBLIC_V4
                .to_string()
                .parse::<Ipv4Addr>()
                .unwrap()
                .to_ipv6_mapped(),
        );
        assert_eq!(classify_ip(mapped_public), AddressClass::Public);
    }

    // ---- the validation matrix ----

    #[test]
    fn https_is_always_allowed_regardless_of_policy_or_address() {
        for policy in [
            InsecureHttpPolicy::Deny,
            InsecureHttpPolicy::AllowPrivateNetwork,
            InsecureHttpPolicy::AllowPublicWithoutCredentials,
        ] {
            for ips in [&[PUBLIC_V4][..], &[PRIVATE_V4][..], &[LOOPBACK_V4][..]] {
                for has_credentials in [true, false] {
                    let result = decide("h", ips, true, has_credentials, policy);
                    assert_eq!(
                        result,
                        Ok(None),
                        "{policy:?} {ips:?} creds={has_credentials}"
                    );
                }
            }
        }
    }

    #[test]
    fn http_to_loopback_is_always_allowed_under_any_policy() {
        for policy in [
            InsecureHttpPolicy::Deny,
            InsecureHttpPolicy::AllowPrivateNetwork,
            InsecureHttpPolicy::AllowPublicWithoutCredentials,
        ] {
            for has_credentials in [true, false] {
                let v4 = decide("127.0.0.1", &[LOOPBACK_V4], false, has_credentials, policy);
                assert_eq!(v4, Ok(None), "{policy:?} creds={has_credentials}");
                let v6 = decide("::1", &[LOOPBACK_V6], false, has_credentials, policy);
                assert_eq!(v6, Ok(None), "{policy:?} creds={has_credentials}");
            }
        }
    }

    #[test]
    fn http_to_private_network_is_denied_under_deny_policy() {
        let error = decide(
            "lan.local",
            &[PRIVATE_V4],
            false,
            false,
            InsecureHttpPolicy::Deny,
        )
        .unwrap_err();
        assert_eq!(
            error,
            OutboundPolicyError::PrivateNetworkRequiresPolicy {
                host: "lan.local".into()
            }
        );
    }

    #[test]
    fn http_to_private_network_is_allowed_with_a_warning_under_allow_private_network() {
        let result = decide(
            "lan.local",
            &[PRIVATE_V4],
            false,
            false,
            InsecureHttpPolicy::AllowPrivateNetwork,
        )
        .unwrap();
        assert_eq!(
            result,
            Some(OutboundWarning::PrivateNetworkHttp {
                host: "lan.local".into()
            })
        );
        assert_eq!(result.unwrap().severity(), OutboundWarningSeverity::Notice);
    }

    #[test]
    fn http_to_public_with_credentials_is_always_denied_no_matter_the_policy() {
        for policy in [
            InsecureHttpPolicy::Deny,
            InsecureHttpPolicy::AllowPrivateNetwork,
            InsecureHttpPolicy::AllowPublicWithoutCredentials,
        ] {
            let error = decide("evil.example", &[PUBLIC_V4], false, true, policy).unwrap_err();
            assert_eq!(
                error,
                OutboundPolicyError::PublicHttpWithCredentialsDenied {
                    host: "evil.example".into()
                },
                "{policy:?} must still deny a credentialed route over plaintext HTTP to a public host"
            );
        }
    }

    #[test]
    fn http_to_public_without_credentials_is_denied_under_deny_and_allow_private_network() {
        for policy in [
            InsecureHttpPolicy::Deny,
            InsecureHttpPolicy::AllowPrivateNetwork,
        ] {
            let error = decide("example.com", &[PUBLIC_V4], false, false, policy).unwrap_err();
            assert_eq!(
                error,
                OutboundPolicyError::PublicHttpRequiresPolicy {
                    host: "example.com".into()
                },
                "{policy:?} must not implicitly allow public HTTP"
            );
        }
    }

    #[test]
    fn http_to_public_without_credentials_is_allowed_with_a_severe_warning_under_its_policy() {
        let result = decide(
            "example.com",
            &[PUBLIC_V4],
            false,
            false,
            InsecureHttpPolicy::AllowPublicWithoutCredentials,
        )
        .unwrap();
        let warning = result.expect("must carry a warning");
        assert_eq!(
            warning,
            OutboundWarning::PublicHttpWithoutCredentials {
                host: "example.com".into()
            }
        );
        assert_eq!(warning.severity(), OutboundWarningSeverity::Severe);
        assert!(
            warning.severity() > OutboundWarningSeverity::Notice,
            "the public-without-credentials warning must outrank the private-network warning"
        );
    }

    #[test]
    fn one_public_answer_among_several_forces_the_whole_host_to_be_treated_as_public() {
        // A hostname with one RFC1918 A record and one real-internet A record
        // must not borrow the private-network exemption through the "safe"
        // looking answer.
        let mixed = [PRIVATE_V4, PUBLIC_V4];
        let error = decide(
            "mixed.example",
            &mixed,
            false,
            false,
            InsecureHttpPolicy::AllowPrivateNetwork,
        )
        .unwrap_err();
        assert_eq!(
            error,
            OutboundPolicyError::PublicHttpRequiresPolicy {
                host: "mixed.example".into()
            }
        );
    }

    #[test]
    fn empty_resolution_is_a_dns_failure_not_a_silent_allow() {
        let error = decide(
            "nowhere.invalid",
            &[],
            false,
            false,
            InsecureHttpPolicy::Deny,
        )
        .unwrap_err();
        assert_eq!(
            error,
            OutboundPolicyError::DnsResolutionFailed {
                host: "nowhere.invalid".into(),
                reason: "no addresses resolved".into()
            }
        );
    }

    // ---- URL parsing entry points ----

    #[test]
    fn validate_outbound_url_sync_allows_https_without_touching_dns() {
        // A host that cannot resolve must not matter for https.
        let result = validate_outbound_url_sync(
            "https://nowhere.invalid.test.vellum/v1",
            true,
            InsecureHttpPolicy::Deny,
        );
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn validate_outbound_url_sync_resolves_a_literal_loopback_ip_without_dns() {
        let result =
            validate_outbound_url_sync("http://127.0.0.1:15721/v1", true, InsecureHttpPolicy::Deny);
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn validate_outbound_url_sync_resolves_a_bracketed_ipv6_loopback_literal() {
        let result =
            validate_outbound_url_sync("http://[::1]:15721/v1", true, InsecureHttpPolicy::Deny);
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn an_unsupported_scheme_is_an_invalid_url_error() {
        let error =
            validate_outbound_url_sync("ftp://127.0.0.1/x", false, InsecureHttpPolicy::Deny)
                .unwrap_err();
        assert!(matches!(error, OutboundPolicyError::InvalidUrl { .. }));
    }

    // ---- DNS is re-resolved, never cached ----

    #[tokio::test]
    async fn dns_is_re_resolved_fresh_so_a_host_can_flip_from_private_to_public_between_calls() {
        // Simulates "admission time" seeing a private-looking answer and
        // "connect time" seeing the attacker's real, repointed public
        // answer. Because `decide` takes freshly resolved IPs every call
        // (there is no cache anywhere in this module), the two calls can
        // and must disagree.
        let admission_time = decide(
            "repointed.example",
            &[PRIVATE_V4],
            false,
            false,
            InsecureHttpPolicy::AllowPrivateNetwork,
        );
        assert!(
            admission_time.is_ok(),
            "admission saw the private-looking answer and allowed it"
        );

        let connect_time = decide(
            "repointed.example",
            &[PUBLIC_V4],
            false,
            false,
            InsecureHttpPolicy::AllowPrivateNetwork,
        );
        assert!(
            connect_time.is_err(),
            "connect time must re-check and refuse the now-public answer, not trust the earlier check"
        );
    }

    #[tokio::test]
    async fn resolve_host_async_returns_a_literal_ip_without_a_real_lookup() {
        let ips = resolve_host_async("127.0.0.1").await.unwrap();
        assert_eq!(ips, vec![LOOPBACK_V4]);
    }

    #[test]
    fn validate_outbound_url_sync_refuses_public_http_with_a_credential() {
        let error = validate_outbound_url_sync(
            "http://1.1.1.1/v1",
            true,
            InsecureHttpPolicy::AllowPublicWithoutCredentials,
        )
        .unwrap_err();
        assert_eq!(
            error,
            OutboundPolicyError::PublicHttpWithCredentialsDenied {
                host: "1.1.1.1".into()
            }
        );
    }

    #[test]
    fn validate_outbound_url_sync_private_ip_is_refused_under_deny_and_admitted_when_allowed() {
        let denied =
            validate_outbound_url_sync("http://10.1.2.3/v1", false, InsecureHttpPolicy::Deny)
                .unwrap_err();
        assert_eq!(
            denied,
            OutboundPolicyError::PrivateNetworkRequiresPolicy {
                host: "10.1.2.3".into()
            }
        );
        let allowed = validate_outbound_url_sync(
            "http://10.1.2.3/v1",
            true,
            InsecureHttpPolicy::AllowPrivateNetwork,
        )
        .unwrap();
        assert_eq!(
            allowed,
            Some(OutboundWarning::PrivateNetworkHttp {
                host: "10.1.2.3".into()
            })
        );
    }

    #[test]
    fn validate_outbound_url_sync_ipv4_mapped_public_does_not_count_as_private() {
        // `::ffff:8.8.8.8` must classify like 8.8.8.8, not sneak through the
        // private-network exemption because it is "IPv6".
        let error = validate_outbound_url_sync(
            "http://[::ffff:8.8.8.8]/v1",
            true,
            InsecureHttpPolicy::AllowPrivateNetwork,
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                OutboundPolicyError::PublicHttpWithCredentialsDenied { .. }
            ),
            "IPv4-mapped public must be refused as public, got {error:?}"
        );
        assert_eq!(
            classify_ip(IpAddr::V6(Ipv4Addr::new(8, 8, 8, 8).to_ipv6_mapped())),
            AddressClass::Public
        );
    }

    fn sample_third_party_route(url: &str) -> crate::route::RuntimeModelRoute {
        crate::route::RuntimeModelRoute {
            route_id: "tp".into(),
            catalog_id: "vlm-tp".into(),
            name: "Third party".into(),
            base_url: url.into(),
            provider_kind: crate::route::RuntimeProviderKind::OpenAiCompatible,
            auth_kind: crate::route::RuntimeAuthKind::Bearer,
            wire: crate::route::RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: false,
            vision: false,
            upstream_model: "m".into(),
            context_window: None,
            reasoning_capabilities: crate::route::RuntimeReasoningCapabilities::default(),
            compaction_capabilities: crate::route::RuntimeCompactionCapabilities::default(),
            compaction_policy: crate::config::RuntimeCompactionPolicy::default(),
            tool_capabilities: crate::route::RuntimeToolCapabilities::default(),
            credential_id: Some("tp".into()),
            insecure_http_policy: InsecureHttpPolicy::Deny,
            provider_profile: None,
            access_mode: None,
            chat_capabilities: crate::route::RuntimeChatCapabilities::default(),
        }
    }

    #[test]
    fn check_third_party_url_refuses_public_http_with_a_credential() {
        let route = sample_third_party_route("http://1.1.1.1/v1");
        let error = check_third_party_url(&route, &route.base_url, true).unwrap_err();
        assert!(matches!(
            error,
            OutboundPolicyError::PublicHttpWithCredentialsDenied { .. }
        ));
    }

    #[test]
    fn check_third_party_url_is_a_no_op_for_official_routes() {
        let mut route = sample_third_party_route("http://1.1.1.1/v1");
        route.provider_kind = crate::route::RuntimeProviderKind::Official;
        // Official dispatch never calls this helper; if it did, the helper
        // itself must not apply the third-party matrix.
        assert_eq!(
            check_third_party_url(&route, &route.base_url, true),
            Ok(None)
        );
        assert!(!stored_route_fails_admission(
            true,
            "http://1.1.1.1/v1",
            true,
            InsecureHttpPolicy::Deny
        ));
    }

    #[test]
    fn official_dispatch_in_exec_does_not_call_the_third_party_validator() {
        let src = include_str!("exec.rs");
        let occurrences = src.matches("check_third_party_url").count();
        assert_eq!(
            occurrences, 1,
            "exec.rs must invoke check_third_party_url at exactly one third-party gate"
        );
        let idx = src.find("check_third_party_url").expect("call site");
        let window = &src[idx.saturating_sub(400)..idx];
        assert!(
            window.contains("RuntimeProviderKind::Official"),
            "the single call site must sit behind an Official exclusion"
        );
        for official_fn in [
            "fn prepare_official_websocket",
            "fn forward_official_compact",
            "fn forward_official_search",
            "fn official_stream",
            "fn official_websocket_endpoint",
        ] {
            let start = src.find(official_fn).unwrap_or_else(|| {
                panic!("{official_fn} must still exist so Official exclusion can be audited")
            });
            let body = &src[start..start.saturating_add(800).min(src.len())];
            assert!(
                !body.contains("check_third_party_url"),
                "{official_fn} must not call the third-party URL validator"
            );
        }
    }
}
