//! Verified against Desktop's bundled Claude Code 2.1.299 (see PR evidence).
use axum::http::HeaderMap;

/// Numeric observations only: never retain raw header values in diagnostics.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct UsageObservation {
    pub five_hour: Option<UsageWindowObservation>,
    pub seven_day: Option<UsageWindowObservation>,
}
#[derive(Clone, Copy, PartialEq)]
pub struct UsageWindowObservation {
    pub utilization: f64,
    pub resets_at: i64,
}
impl UsageObservation {
    pub(super) fn from_headers(headers: &HeaderMap) -> Self {
        let window = |used: &str, reset: &str| {
            // Reject ambiguous duplicates and incomplete windows. Missing data
            // must never manufacture a fresh zero or erase the API snapshot.
            if headers.get_all(used).iter().count() != 1
                || headers.get_all(reset).iter().count() != 1
            {
                return None;
            }
            let fraction = headers.get(used)?.to_str().ok()?.parse::<f64>().ok()?;
            let seconds = headers.get(reset)?.to_str().ok()?.parse::<i64>().ok()?;
            if !fraction.is_finite()
                || fraction < 0.0
                || seconds <= 0
                || chrono::DateTime::from_timestamp(seconds, 0).is_none()
            {
                return None;
            }
            Some(UsageWindowObservation {
                // The bundled client documents legitimate values above one.
                // Gates use percent remaining and saturate at exhaustion.
                utilization: fraction.min(1.0) * 100.0,
                resets_at: seconds,
            })
        };
        Self {
            five_hour: window(
                "anthropic-ratelimit-unified-5h-utilization",
                "anthropic-ratelimit-unified-5h-reset",
            ),
            seven_day: window(
                "anthropic-ratelimit-unified-7d-utilization",
                "anthropic-ratelimit-unified-7d-reset",
            ),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.seven_day.is_none()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verified_fraction_and_epoch_units_and_invalid_windows() {
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.85".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-5h-reset",
            "1900000000".parse().unwrap(),
        );
        let w = UsageObservation::from_headers(&h).five_hour.unwrap();
        assert_eq!(w.utilization, 85.0);
        assert_eq!(w.resets_at, 1900000000);
        for (input, expected) in [
            ("1.2", Some(100.0)),
            ("NaN", None),
            ("inf", None),
            ("-0.1", None),
            ("bad", None),
        ] {
            h.insert(
                "anthropic-ratelimit-unified-5h-utilization",
                input.parse().unwrap(),
            );
            assert_eq!(
                UsageObservation::from_headers(&h)
                    .five_hour
                    .map(|w| w.utilization),
                expected
            );
        }
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.5".parse().unwrap(),
        );
        for reset in ["2026-10-20T00:00:00Z", "-1", "9223372036854775807", "bad"] {
            h.insert(
                "anthropic-ratelimit-unified-5h-reset",
                reset.parse().unwrap(),
            );
            assert!(UsageObservation::from_headers(&h).is_empty());
        }
        h.insert(
            "anthropic-ratelimit-unified-5h-reset",
            "1900000000".parse().unwrap(),
        );
        h.append(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.2".parse().unwrap(),
        );
        assert!(UsageObservation::from_headers(&h).is_empty());
        assert!(UsageObservation::from_headers(&HeaderMap::new()).is_empty());
    }
}
