//! Production resource policy (V-03).
//!
//! Four independent budgets protect a shared proxy from unbounded admission:
//! a global buffered-byte pool, a long-task HTTP request semaphore, a
//! third-party stream semaphore, and Official WebSocket slots. Production
//! constructors install the defaults below. Runtime config may *lower* a
//! value; nothing, including a provider response, may raise one.
//!
//! `/health` and `/readyz` stay authenticated at the inbound guard but must
//! not take the long-task HTTP semaphore. Shortage answers with the
//! canonical retryable [`crate::error::RuntimeError::ResourceExhausted`]
//! envelope (HTTP 429), never raw Axum/Tower text.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use futures_util::StreamExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::ResourceLimitsConfig;
use crate::error::RuntimeError;

pub const DEFAULT_BUFFERED_BYTE_BUDGET: usize = 512 * 1024 * 1024;
pub const DEFAULT_ACTIVE_HTTP_REQUESTS: usize = 32;
pub const DEFAULT_THIRD_PARTY_STREAMS: usize = 16;
pub const DEFAULT_OFFICIAL_WS_SLOTS: usize = 64;

/// One production resource policy. Cheap to clone: every clone shares the
/// same semaphores.
#[derive(Clone)]
pub struct ResourcePolicy {
    inner: Arc<ResourcePolicyInner>,
}

struct ResourcePolicyInner {
    buffered_bytes: Arc<Semaphore>,
    http_requests: Arc<Semaphore>,
    third_party_streams: Arc<Semaphore>,
    official_ws: Arc<Semaphore>,
    buffered_byte_budget: usize,
    active_http_requests: usize,
    third_party_stream_limit: usize,
    official_ws_slots: usize,
}

/// Held for the lifetime of one admitted slot. Dropping it always releases
/// the permit, including on panic/early return.
#[derive(Debug)]
pub struct ResourceGuard {
    _permit: OwnedSemaphorePermit,
}

/// Held for the lifetime of one set of buffered bytes. Dropping it returns
/// those bytes to the global budget.
#[derive(Debug)]
pub struct BufferedBytesGuard {
    permits: Vec<OwnedSemaphorePermit>,
}

impl BufferedBytesGuard {
    fn empty() -> Self {
        Self {
            permits: Vec::new(),
        }
    }
}

/// Why collecting an inbound body failed.
#[derive(Debug)]
pub enum CollectBodyError {
    WireTooLarge,
    Resource(RuntimeError),
    Read(String),
}

impl ResourcePolicy {
    /// Production defaults. The only constructor a production router may
    /// call without an explicit lower-limit config.
    pub fn production() -> Self {
        Self::with_limits(
            DEFAULT_BUFFERED_BYTE_BUDGET,
            DEFAULT_ACTIVE_HTTP_REQUESTS,
            DEFAULT_THIRD_PARTY_STREAMS,
            DEFAULT_OFFICIAL_WS_SLOTS,
        )
    }

    /// Build a policy, clamping every value so it can only be lowered from
    /// the production default.
    pub fn with_limits(
        buffered_byte_budget: usize,
        active_http_requests: usize,
        third_party_streams: usize,
        official_ws_slots: usize,
    ) -> Self {
        let buffered_byte_budget = buffered_byte_budget.clamp(1, DEFAULT_BUFFERED_BYTE_BUDGET);
        let active_http_requests = active_http_requests.clamp(1, DEFAULT_ACTIVE_HTTP_REQUESTS);
        let third_party_streams = third_party_streams.clamp(1, DEFAULT_THIRD_PARTY_STREAMS);
        let official_ws_slots = official_ws_slots.clamp(1, DEFAULT_OFFICIAL_WS_SLOTS);
        Self {
            inner: Arc::new(ResourcePolicyInner {
                buffered_bytes: Arc::new(Semaphore::new(buffered_byte_budget)),
                http_requests: Arc::new(Semaphore::new(active_http_requests)),
                third_party_streams: Arc::new(Semaphore::new(third_party_streams)),
                official_ws: Arc::new(Semaphore::new(official_ws_slots)),
                buffered_byte_budget,
                active_http_requests,
                third_party_stream_limit: third_party_streams,
                official_ws_slots,
            }),
        }
    }

    /// Apply optional config overrides. Missing fields keep the production
    /// default; values above the default are clamped down.
    pub fn from_config(config: &ResourceLimitsConfig) -> Self {
        Self::with_limits(
            config
                .buffered_byte_budget
                .unwrap_or(DEFAULT_BUFFERED_BYTE_BUDGET),
            config
                .active_http_requests
                .unwrap_or(DEFAULT_ACTIVE_HTTP_REQUESTS),
            config
                .third_party_streams
                .unwrap_or(DEFAULT_THIRD_PARTY_STREAMS),
            config
                .official_ws_slots
                .unwrap_or(DEFAULT_OFFICIAL_WS_SLOTS),
        )
    }

    pub fn buffered_byte_budget(&self) -> usize {
        self.inner.buffered_byte_budget
    }

    pub fn active_http_requests(&self) -> usize {
        self.inner.active_http_requests
    }

    pub fn third_party_stream_limit(&self) -> usize {
        self.inner.third_party_stream_limit
    }

    pub fn official_ws_slots(&self) -> usize {
        self.inner.official_ws_slots
    }

    pub fn try_acquire_http(&self) -> Result<ResourceGuard, RuntimeError> {
        acquire_one(
            &self.inner.http_requests,
            "http_requests",
            self.inner.active_http_requests,
        )
    }

    pub fn try_acquire_third_party_stream(&self) -> Result<ResourceGuard, RuntimeError> {
        acquire_one(
            &self.inner.third_party_streams,
            "third_party_streams",
            self.inner.third_party_stream_limit,
        )
    }

    pub fn try_acquire_official_ws(&self) -> Result<ResourceGuard, RuntimeError> {
        acquire_one(
            &self.inner.official_ws,
            "official_websocket",
            self.inner.official_ws_slots,
        )
    }

    /// Acquire `bytes` from the global buffered-byte budget *before* the
    /// caller keeps those bytes. Release happens when the returned guard
    /// drops.
    pub fn try_acquire_buffered_bytes(
        &self,
        bytes: usize,
    ) -> Result<BufferedBytesGuard, RuntimeError> {
        if bytes == 0 {
            return Ok(BufferedBytesGuard::empty());
        }
        if bytes > self.inner.buffered_byte_budget {
            return Err(resource_exhausted(
                "buffered_bytes",
                self.inner.buffered_byte_budget,
            ));
        }
        let n = u32::try_from(bytes)
            .map_err(|_| resource_exhausted("buffered_bytes", self.inner.buffered_byte_budget))?;
        match self.inner.buffered_bytes.clone().try_acquire_many_owned(n) {
            Ok(permit) => Ok(BufferedBytesGuard {
                permits: vec![permit],
            }),
            Err(_) => Err(resource_exhausted(
                "buffered_bytes",
                self.inner.buffered_byte_budget,
            )),
        }
    }
}

fn acquire_one(
    semaphore: &Arc<Semaphore>,
    name: &str,
    limit: usize,
) -> Result<ResourceGuard, RuntimeError> {
    match semaphore.clone().try_acquire_owned() {
        Ok(permit) => Ok(ResourceGuard { _permit: permit }),
        Err(_) => Err(resource_exhausted(name, limit)),
    }
}

fn resource_exhausted(name: &str, limit: usize) -> RuntimeError {
    RuntimeError::ResourceExhausted(format!("no remaining {name} capacity (limit={limit})"))
}

/// Read an inbound HTTP body under the identity size cap *and* the global
/// buffered-byte budget. Permits are acquired for each chunk before it is
/// kept and released when the returned guard drops.
pub async fn collect_request_body(
    body: Body,
    max_bytes: usize,
    policy: &ResourcePolicy,
) -> Result<(Bytes, BufferedBytesGuard), CollectBodyError> {
    let mut collected = Vec::new();
    let mut guard = BufferedBytesGuard::empty();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| CollectBodyError::Read(error.to_string()))?;
        let next = collected.len().saturating_add(chunk.len());
        if next > max_bytes {
            return Err(CollectBodyError::WireTooLarge);
        }
        if !chunk.is_empty() {
            let extra = policy
                .try_acquire_buffered_bytes(chunk.len())
                .map_err(CollectBodyError::Resource)?;
            guard.permits.extend(extra.permits);
        }
        collected.extend_from_slice(&chunk);
    }
    Ok((Bytes::from(collected), guard))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_defaults_are_the_documented_caps() {
        let policy = ResourcePolicy::production();
        assert_eq!(policy.buffered_byte_budget(), DEFAULT_BUFFERED_BYTE_BUDGET);
        assert_eq!(policy.active_http_requests(), DEFAULT_ACTIVE_HTTP_REQUESTS);
        assert_eq!(
            policy.third_party_stream_limit(),
            DEFAULT_THIRD_PARTY_STREAMS
        );
        assert_eq!(policy.official_ws_slots(), DEFAULT_OFFICIAL_WS_SLOTS);
    }

    #[test]
    fn config_may_lower_limits_but_never_raise_them() {
        let lowered = ResourcePolicy::with_limits(1024, 2, 1, 4);
        assert_eq!(lowered.buffered_byte_budget(), 1024);
        assert_eq!(lowered.active_http_requests(), 2);
        assert_eq!(lowered.third_party_stream_limit(), 1);
        assert_eq!(lowered.official_ws_slots(), 4);

        let raised = ResourcePolicy::with_limits(
            DEFAULT_BUFFERED_BYTE_BUDGET * 2,
            DEFAULT_ACTIVE_HTTP_REQUESTS * 2,
            DEFAULT_THIRD_PARTY_STREAMS * 2,
            DEFAULT_OFFICIAL_WS_SLOTS * 2,
        );
        assert_eq!(raised.buffered_byte_budget(), DEFAULT_BUFFERED_BYTE_BUDGET);
        assert_eq!(raised.active_http_requests(), DEFAULT_ACTIVE_HTTP_REQUESTS);
        assert_eq!(
            raised.third_party_stream_limit(),
            DEFAULT_THIRD_PARTY_STREAMS
        );
        assert_eq!(raised.official_ws_slots(), DEFAULT_OFFICIAL_WS_SLOTS);
    }

    #[test]
    fn filling_the_http_semaphore_returns_canonical_resource_exhausted() {
        let policy = ResourcePolicy::production();
        let _held: Vec<_> = (0..policy.active_http_requests())
            .map(|_| policy.try_acquire_http().expect("slot available"))
            .collect();
        let error = policy.try_acquire_http().expect_err("pool is full");
        assert_eq!(error.category(), "resource_exhausted");
        assert_eq!(error.http_status().as_u16(), 429);
        let payload = error.to_error_payload();
        assert_eq!(payload["error"]["type"], "vellum_proxy_error");
        assert_eq!(payload["error"]["category"], "resource_exhausted");
        assert_eq!(payload["error"]["code"], 429);
    }

    #[test]
    fn websocket_slots_are_independent_of_http() {
        let policy = ResourcePolicy::with_limits(
            DEFAULT_BUFFERED_BYTE_BUDGET,
            1,
            DEFAULT_THIRD_PARTY_STREAMS,
            2,
        );
        let _http = policy.try_acquire_http().unwrap();
        assert!(policy.try_acquire_http().is_err());
        let _ws = policy.try_acquire_official_ws().unwrap();
        let _ws2 = policy.try_acquire_official_ws().unwrap();
        assert!(policy.try_acquire_official_ws().is_err());
    }

    #[tokio::test]
    async fn collect_request_body_acquires_before_keeping_bytes() {
        let policy = ResourcePolicy::with_limits(8, 1, 1, 1);
        let (bytes, _guard) = collect_request_body(Body::from(vec![1, 2, 3, 4]), 300, &policy)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), &[1, 2, 3, 4]);
        let error = policy
            .try_acquire_buffered_bytes(8)
            .expect_err("4 bytes still held");
        assert_eq!(error.category(), "resource_exhausted");
    }
}
