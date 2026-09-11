//! Regression: a third-party provider must not be able to panic Vellum's
//! streaming task, or slip a malformed known event past validation, with a
//! hostile SSE frame.
//!
//! Originally a security-review POC. `parse_sse_value` takes the event name
//! from the SSE `event:` field and the payload from `data:` as an arbitrary
//! `serde_json::Value` — it never required the payload to be a JSON *object*,
//! while several handlers in `ThirdPartySseNormalizer::push_block` then called
//! `value.as_object_mut()` and `.expect(...)` on the result. A panic there
//! killed the tokio task carrying the user's stream.
//!
//! `ThirdPartySseNormalizer` is the production third-party streaming path
//! (`exec.rs`, inside `third_party_stream`), and every case below is driven
//! through the real public API with no mocking.

use serde_json::json;
use vellum_proxy_runtime::streaming::ThirdPartySseNormalizer;
use vellum_proxy_runtime::{SseProtocolError, ThirdPartySseNormalizer as ExportedAlias};

/// Feed one SSE block to a fresh normalizer, reporting a panic as an `Err` so
/// the distinction between "panicked" and "returned a protocol error" stays
/// visible in the assertions below.
fn feed(block: &str) -> Result<Result<Vec<String>, SseProtocolError>, String> {
    let outcome = std::panic::catch_unwind(|| {
        let request = json!({"model": "any", "stream": true});
        let mut normalizer = ThirdPartySseNormalizer::new(&request, false);
        normalizer.push_block(block)
    });
    outcome.map_err(|payload| {
        payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic payload".into())
    })
}

/// Run `body` with the default panic hook silenced, so an intentionally
/// panicking case does not spray the test output.
fn without_panic_noise<T>(body: impl FnOnce() -> T) -> T {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = body();
    std::panic::set_hook(previous);
    outcome
}

#[test]
fn non_object_payload_on_a_known_event_is_a_protocol_error_not_a_panic() {
    // Keep the exported alias referenced so this also proves the type is
    // reachable from outside the crate exactly as production uses it.
    let _: fn(&serde_json::Value, bool) -> ExportedAlias = ExportedAlias::new;

    // The original hostile frame. Two lines. No authentication, no size,
    // nothing exotic — just a `data:` payload that is a JSON string instead of
    // an object, under an event name the normalizer rewrites in place.
    let hostile = "event: response.reasoning_summary_text.done\ndata: \"not-an-object\"";
    let outcome = without_panic_noise(|| feed(hostile));

    let result = outcome.expect("the hostile frame must not panic the streaming task");
    let error = result.expect_err("a malformed known event must not be forwarded");
    assert_eq!(error.event(), "response.reasoning_summary_text.done");
    assert!(
        error.detail().contains("object"),
        "the diagnostic must name the shape problem, got: {}",
        error.detail()
    );
}

#[test]
fn the_same_event_with_a_well_formed_payload_is_still_normalized() {
    let benign = "event: response.reasoning_summary_text.done\ndata: {\"text\":\"hello\"}";
    let blocks = feed(benign)
        .expect("no panic")
        .expect("a well-formed known event is not a protocol error");
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].contains("response.reasoning_summary_text.done"));
}

#[test]
fn unknown_events_keep_their_passthrough_behavior() {
    // The protocol reserves `response.*` and `error`. Anything else is
    // provider-specific and must survive untouched regardless of payload shape,
    // which is what the passthrough contract promises.
    for block in [
        "event: provider.telemetry\ndata: \"not-an-object\"",
        "event: provider.telemetry\ndata: [1,2,3]",
        "data: 42",
    ] {
        let blocks = feed(block)
            .expect("no panic")
            .unwrap_or_else(|error| panic!("unknown event must pass through, got {error}"));
        assert_eq!(blocks.len(), 1, "block {block:?} should pass through");
    }
}

#[test]
fn a_well_formed_reasoning_delta_cannot_be_followed_by_a_scalar_frame() {
    // A provider could send one well-formed reasoning delta, then a scalar
    // payload under the same event, trying to reach a handler with an
    // `expect`/`.as_object()` assumption on a non-object value.
    let mut normalizer = ThirdPartySseNormalizer::new(&json!({}), false);
    let primed = normalizer
        .push_block(
            "event: response.reasoning_summary_text.delta\n\
             data: {\"output_index\":0,\"summary_index\":0,\"delta\":\"Checking the stream.\"}",
        )
        .expect("well-formed delta");
    assert_eq!(primed.len(), 1);

    let outcome = without_panic_noise(|| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            normalizer.push_block("event: response.reasoning_summary_text.delta\ndata: \"scalar\"")
        }))
    });
    let result = outcome.expect("the follow-up scalar frame must not panic");
    assert!(
        result.is_err(),
        "a scalar payload under a known event must be a protocol error"
    );
}

#[test]
fn malformed_nested_containers_and_identities_are_rejected() {
    // Each of these would otherwise be read with a silent fallback: a scalar
    // `item`/`response` skips the rewrite, a non-integer `sequence_number`
    // disables replay suppression, and a non-integer `output_index` folds
    // distinct items onto index 0.
    let cases = [
        (
            "event: response.output_item.done\ndata: {\"item\":\"not-an-object\"}",
            "item",
        ),
        (
            "event: response.completed\ndata: {\"response\":[1,2]}",
            "response",
        ),
        (
            "event: response.output_text.delta\ndata: {\"delta\":{\"nested\":true}}",
            "delta",
        ),
        (
            "event: response.output_text.delta\ndata: {\"delta\":\"hi\",\"sequence_number\":1.5}",
            "sequence_number",
        ),
        (
            "event: response.output_text.delta\ndata: {\"delta\":\"hi\",\"output_index\":\"0\"}",
            "output_index",
        ),
    ];
    for (block, field) in cases {
        let error = feed(block)
            .expect("no panic")
            .expect_err("malformed known event must be rejected");
        assert!(
            error.detail().contains(field),
            "expected the diagnostic to name `{field}`, got: {}",
            error.detail()
        );
    }
}

#[test]
fn protocol_diagnostics_are_bounded_and_single_line() {
    // The diagnostic carries provider-influenced text (the event name), so it
    // must not be a channel for unbounded or multi-line provider content.
    let long_event = format!("response.{}", "x".repeat(4096));
    let block = format!("event: {long_event}\ndata: \"scalar\"");
    let error = feed(&block)
        .expect("no panic")
        .expect_err("malformed known event must be rejected");

    let rendered = error.to_string();
    assert!(
        rendered.chars().count() < 512,
        "diagnostic must stay bounded, got {} chars",
        rendered.chars().count()
    );
    assert!(
        !rendered.contains('\n') && !rendered.contains('\r'),
        "diagnostic must stay on one line"
    );
}
