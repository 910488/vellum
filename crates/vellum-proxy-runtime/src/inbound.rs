//! Inbound access policy for the local proxy boundary (V-01/V-02/V-10).
//!
//! The proxy listens on loopback and executes the user's configured routes with
//! the user's stored credentials. Loopback is not an authorization boundary:
//! any page the user visits can reach `127.0.0.1`, and a WebSocket upgrade is
//! not subject to the same-origin policy — the browser sends `Origin` and
//! expects the *server* to reject foreign ones. Every other local process
//! running as the user can reach it too.
//!
//! So every endpoint, `/health` and `/readyz` included, requires a boundary
//! key. The threat model this defends is browsers, other OS users, and
//! unauthorized local services. It does not claim to defend against code
//! already running under the same OS account, which can read the key from the
//! same place the proxy does.

use std::fmt;

use axum::http::header::{HOST, ORIGIN};
use axum::http::{HeaderMap, Uri};

use crate::error::RuntimeError;

/// The header Codex and every first-party caller present the boundary key in.
pub const BOUNDARY_KEY_HEADER: &str = "x-vellum-boundary-key";

/// The reserved credential ID the boundary key is stored under. Local installs
/// keep it in the Vellum credential store; Remote mounts it under
/// `/run/secrets`.
pub const BOUNDARY_CREDENTIAL_ID: &str = "__vellum_proxy_boundary__";

/// Key length before encoding. 32 bytes of CSPRNG output.
pub const BOUNDARY_KEY_BYTES: usize = 32;

/// Environment variable a remote-managed Codex reads the boundary key from.
///
/// Remote Codex's `config.toml` cannot carry the raw secret the way Desktop's
/// `http_headers` does: that file is diffed into the deployment plan and the
/// per-injection lease (see `vellum-remote-agent`'s `desired_values`/lease
/// JSON), both of which are inspectable outside the trust boundary that holds
/// the secret itself. `env_http_headers` instead takes an *environment
/// variable name* as its TOML value — never the secret — and Codex resolves
/// the header value from that variable in its own process environment at
/// request time. The remote agent is the one that both provisions this value
/// (`vellum-remote-agent::configuration::read_boundary_key`) and sets it on
/// the environment of the process that starts Codex's daemon.
pub const BOUNDARY_KEY_ENV_VAR: &str = "VELLUM_BOUNDARY_KEY";

/// A 32-byte boundary key, held as its lowercase hex encoding.
///
/// Never `Display`, never `Serialize`, and its `Debug` is redacted, so the key
/// cannot reach a log line, a diagnostic payload, an error body, or a
/// deployment plan by being formatted somewhere.
#[derive(Clone, PartialEq, Eq)]
pub struct BoundaryKey(String);

impl BoundaryKey {
    /// Draw a fresh key from the OS CSPRNG.
    pub fn generate() -> Result<Self, String> {
        let mut bytes = [0u8; BOUNDARY_KEY_BYTES];
        getrandom::fill(&mut bytes)
            .map_err(|error| format!("failed to draw a boundary key: {error}"))?;
        let hex = bytes.iter().fold(
            String::with_capacity(BOUNDARY_KEY_BYTES * 2),
            |mut encoded, byte| {
                use fmt::Write as _;
                let _ = write!(encoded, "{byte:02x}");
                encoded
            },
        );
        Ok(Self(hex))
    }

    /// Accept a stored key. Rejects anything that is not the exact encoding
    /// this runtime writes, so a truncated or placeholder secret fails at
    /// startup instead of silently weakening the boundary.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.len() != BOUNDARY_KEY_BYTES * 2 {
            return Err(format!(
                "boundary key must be {} hex characters, got {}",
                BOUNDARY_KEY_BYTES * 2,
                trimmed.len()
            ));
        }
        if !trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("boundary key must be hex".into());
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    /// The encoded key, for writing to the credential store or the Codex
    /// provider header. Deliberately awkward to reach by accident.
    pub fn expose_for_storage(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison against a presented value.
    fn verify(&self, presented: &str) -> bool {
        constant_time_eq(self.0.as_bytes(), presented.trim().as_bytes())
    }
}

impl fmt::Debug for BoundaryKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BoundaryKey(<redacted>)")
    }
}

/// Compare two byte strings without an early exit on the first difference.
///
/// The length of a boundary key is fixed and public, so comparing lengths up
/// front leaks nothing. `black_box` keeps the accumulator from being optimized
/// into a short-circuit.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    std::hint::black_box(difference) == 0
}

/// How the proxy admits inbound callers.
#[derive(Debug, Clone)]
pub enum InboundAccessPolicy {
    /// The only policy a production constructor may use.
    Authenticated {
        /// Where the key came from. Recorded in the Desktop lease so a restart
        /// can tell whether it is still managing its own proxy.
        credential_id: String,
        key: BoundaryKey,
        /// Exact `Host` values this proxy answers to, including the port.
        allowed_hosts: Vec<String>,
        /// Reject any request carrying an `Origin` header. No Codex HTTP or
        /// WebSocket client sends one; a browser always does.
        reject_origin: bool,
    },
    /// Compiled only for unit tests and the explicit testkit, so no production
    /// build can name it.
    #[cfg(any(test, feature = "testkit"))]
    TestOnlyDisabled,
}

impl InboundAccessPolicy {
    /// The production constructor. `port` is the port the proxy actually bound,
    /// so a request addressed to a different local service's port is refused
    /// even if it reaches this listener.
    pub fn authenticated(credential_id: impl Into<String>, key: BoundaryKey, port: u16) -> Self {
        Self::Authenticated {
            credential_id: credential_id.into(),
            key,
            allowed_hosts: loopback_hosts(port),
            reject_origin: true,
        }
    }

    /// Disable inbound authentication. Unit tests and the explicit testkit
    /// only; [`Self::authenticated`] is the production path.
    #[cfg(any(test, feature = "testkit"))]
    pub fn test_only_disabled() -> Self {
        Self::TestOnlyDisabled
    }

    /// The credential ID backing this policy, when it is authenticated.
    pub fn credential_id(&self) -> Option<&str> {
        match self {
            Self::Authenticated { credential_id, .. } => Some(credential_id),
            #[cfg(any(test, feature = "testkit"))]
            Self::TestOnlyDisabled => None,
        }
    }

    /// Admit or refuse one request.
    ///
    /// Host and Origin are checked before the key so that a browser probing the
    /// port gets a shape error rather than a signal about key validity, and so
    /// the reason surfaced to the user names the actual problem.
    pub fn check(&self, headers: &HeaderMap, uri: &Uri) -> Result<(), RuntimeError> {
        // Exhaustive under both cfgs. In a production build `Authenticated` is
        // the only variant, which is the point: there is nothing else to match.
        let (key, allowed_hosts, reject_origin) = match self {
            Self::Authenticated {
                key,
                allowed_hosts,
                reject_origin,
                ..
            } => (key, allowed_hosts, reject_origin),
            #[cfg(any(test, feature = "testkit"))]
            Self::TestOnlyDisabled => return Ok(()),
        };

        if *reject_origin && headers.contains_key(ORIGIN) {
            // Deliberately does not echo the origin back: it is
            // attacker-controlled and this message reaches a log.
            return Err(RuntimeError::InvalidRequest(
                "this endpoint does not accept requests carrying an Origin header".into(),
            ));
        }

        let host = headers
            .get(HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .or_else(|| uri.authority().map(|authority| authority.to_string()));
        let Some(host) = host else {
            return Err(RuntimeError::InvalidRequest(
                "requests must address this proxy by its loopback host".into(),
            ));
        };
        if !allowed_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host.trim()))
        {
            return Err(RuntimeError::InvalidRequest(
                "requests must address this proxy by its loopback host".into(),
            ));
        }

        let presented = headers
            .get(BOUNDARY_KEY_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !key.verify(presented) {
            return Err(RuntimeError::AuthenticationFailed(
                "missing or invalid Vellum proxy boundary key".into(),
            ));
        }
        Ok(())
    }
}

/// The exact `Host` values a loopback listener on `port` answers to.
fn loopback_hosts(port: u16) -> Vec<String> {
    vec![
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> InboundAccessPolicy {
        InboundAccessPolicy::authenticated(
            BOUNDARY_CREDENTIAL_ID,
            BoundaryKey::parse(&"a1".repeat(32)).unwrap(),
            15721,
        )
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    fn uri() -> Uri {
        "/v1/responses".parse().unwrap()
    }

    #[test]
    fn a_correct_key_from_a_loopback_host_is_admitted() {
        for host in ["127.0.0.1:15721", "localhost:15721", "[::1]:15721"] {
            let result = policy().check(
                &headers(&[("host", host), (BOUNDARY_KEY_HEADER, &"a1".repeat(32))]),
                &uri(),
            );
            assert!(result.is_ok(), "{host} must be admitted: {result:?}");
        }
    }

    #[test]
    fn a_missing_or_wrong_key_is_authentication_failed() {
        let wrong = "b2".repeat(32);
        let cases = [
            vec![("host", "127.0.0.1:15721")],
            vec![("host", "127.0.0.1:15721"), (BOUNDARY_KEY_HEADER, "")],
            vec![("host", "127.0.0.1:15721"), (BOUNDARY_KEY_HEADER, &wrong)],
            // A correct prefix must not help.
            vec![
                ("host", "127.0.0.1:15721"),
                (BOUNDARY_KEY_HEADER, "a1a1a1a1"),
            ],
        ];
        for case in cases {
            let error = policy().check(&headers(&case), &uri()).unwrap_err();
            assert_eq!(error.category(), "authentication_failed", "{case:?}");
            assert_eq!(error.http_status().as_u16(), 401);
        }
    }

    #[test]
    fn any_origin_at_all_is_rejected_before_the_key_is_considered() {
        // This is the cross-site WebSocket hijack: the attacker's page cannot
        // read the key, but it must not even get as far as being told so.
        let error = policy()
            .check(
                &headers(&[
                    ("host", "127.0.0.1:15721"),
                    ("origin", "https://evil.example"),
                    (BOUNDARY_KEY_HEADER, &"a1".repeat(32)),
                ]),
                &uri(),
            )
            .unwrap_err();
        assert_eq!(error.category(), "invalid_request");
        assert_eq!(error.http_status().as_u16(), 400);
        assert!(
            !error.to_string().contains("evil.example"),
            "an attacker-controlled origin must not be echoed into a log line"
        );
    }

    #[test]
    fn a_foreign_or_absent_host_is_invalid_request() {
        let key = "a1".repeat(32);
        let cases: Vec<Vec<(&str, &str)>> = vec![
            vec![(BOUNDARY_KEY_HEADER, key.as_str())],
            vec![("host", "vellum.example"), (BOUNDARY_KEY_HEADER, "a")],
            // A different local service's port on the same interface.
            vec![("host", "127.0.0.1:1234"), (BOUNDARY_KEY_HEADER, "a")],
            // DNS rebinding target that resolves to loopback.
            vec![
                ("host", "attacker.example:15721"),
                (BOUNDARY_KEY_HEADER, "a"),
            ],
        ];
        for case in cases {
            let error = policy().check(&headers(&case), &uri()).unwrap_err();
            assert_eq!(error.category(), "invalid_request", "{case:?}");
            assert_eq!(error.http_status().as_u16(), 400);
        }
    }

    #[test]
    fn generated_keys_are_full_length_hex_and_distinct() {
        let first = BoundaryKey::generate().unwrap();
        let second = BoundaryKey::generate().unwrap();
        assert_eq!(first.expose_for_storage().len(), BOUNDARY_KEY_BYTES * 2);
        assert!(first
            .expose_for_storage()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn a_short_or_non_hex_stored_key_fails_to_load() {
        assert!(BoundaryKey::parse("").is_err());
        assert!(BoundaryKey::parse("not-a-key").is_err());
        assert!(BoundaryKey::parse(&"a1".repeat(16)).is_err());
        assert!(BoundaryKey::parse(&"zz".repeat(32)).is_err());
        assert!(BoundaryKey::parse(&format!("  {}  ", "A1".repeat(32))).is_ok());
    }

    #[test]
    fn the_key_never_renders_itself() {
        let key = BoundaryKey::parse(&"a1".repeat(32)).unwrap();
        let rendered = format!("{key:?}");
        assert!(
            !rendered.contains("a1a1"),
            "Debug leaked the key: {rendered}"
        );

        let policy = policy();
        let rendered = format!("{policy:?}");
        assert!(
            !rendered.contains("a1a1"),
            "the policy's Debug leaked the key: {rendered}"
        );
    }

    #[test]
    fn constant_time_eq_matches_ordinary_equality() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }
}
