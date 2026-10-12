//! Emit transitions, not continuously changing quota measurements.
use std::sync::Arc;
use vellum_proxy_runtime::{DiagnosticEvent, DiagnosticsSink, NoopSink, QuotaPoolDecision};

type DecisionFingerprint = (Option<String>, Vec<(String, Option<String>)>);

pub(crate) struct DecisionObserver {
    sink: Arc<dyn DiagnosticsSink>,
    sink_installed: bool,
    previous: Option<DecisionFingerprint>,
}

impl Default for DecisionObserver {
    fn default() -> Self {
        Self {
            sink: Arc::new(NoopSink),
            sink_installed: false,
            previous: None,
        }
    }
}

impl DecisionObserver {
    pub fn set_sink(&mut self, sink: Arc<dyn DiagnosticsSink>) {
        self.sink = sink;
        if !self.sink_installed {
            self.previous = None;
            self.sink_installed = true;
        }
    }

    pub fn record(&mut self, mut decision: QuotaPoolDecision) {
        decision
            .candidates
            .sort_by(|a, b| a.account_hash.cmp(&b.account_hash));
        let fingerprint = (
            decision.selected_account_hash.clone(),
            decision
                .candidates
                .iter()
                .map(|c| (c.account_hash.clone(), c.exclusion_reason.clone()))
                .collect(),
        );
        if self.previous.as_ref() == Some(&fingerprint) {
            return;
        }
        self.previous = Some(fingerprint);
        if let Ok(encoded) = serde_json::to_string(&decision) {
            log::info!("[QuotaPoolDecision] {encoded}");
        }
        self.sink
            .record(DiagnosticEvent::QuotaPoolDecision(decision));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use vellum_proxy_runtime::{DetailLevel, QuotaPoolCandidateDiagnostic};
    #[derive(Default)]
    struct Sink(Mutex<Vec<DiagnosticEvent>>);
    impl DiagnosticsSink for Sink {
        fn record(&self, event: DiagnosticEvent) {
            self.0.lock().unwrap().push(event);
        }
        fn detail_level(&self) -> DetailLevel {
            DetailLevel::T1Summary
        }
    }
    #[test]
    fn writes_only_selection_or_exclusion_transitions() {
        let sink = Arc::new(Sink::default());
        let mut observer = DecisionObserver::default();
        observer.set_sink(sink.clone());
        let mut d = QuotaPoolDecision {
            selected_account_hash: Some("hashed-a".into()),
            reason: "new_session_min_util".into(),
            candidates: vec![QuotaPoolCandidateDiagnostic {
                account_hash: "hashed-b".into(),
                exclusion_reason: Some("weekly_floor".into()),
                headroom: Some(0.0),
                weekly_used: Some(90.0),
                five_hour_used: None,
                weekly_reset_at: Some(100),
                five_hour_reset_at: None,
                tokens_per_point: 10_000.0,
                capacity_source: "prior".into(),
                external_points: 0.0,
                low_sample_count: 0,
                five_hour_tokens_per_point: None,
                five_hour_capacity_source: None,
                five_hour_external_points: 0.0,
                five_hour_low_sample_count: 0,
                request_budget_fits: None,
                request_budget_enforced: false,
            }],
        };
        observer.record(d.clone());
        d.candidates[0].weekly_used = Some(91.0);
        d.reason = "stay_cache_hot".into();
        observer.record(d.clone());
        assert_eq!(sink.0.lock().unwrap().len(), 1);
        // Rebuilding the runtime's sink must not duplicate the same decision.
        observer.set_sink(sink.clone());
        observer.record(d.clone());
        assert_eq!(sink.0.lock().unwrap().len(), 1);
        d.candidates[0].exclusion_reason = Some("five_hour_reserve".into());
        observer.record(d.clone());
        observer.record(d.clone());
        assert_eq!(sink.0.lock().unwrap().len(), 2);
        d.selected_account_hash = Some("hashed-b".into());
        observer.record(d);
        assert_eq!(sink.0.lock().unwrap().len(), 3);

        // Exercise the production sink and SQLite/support-export seam too.
        let temp = tempfile::tempdir().unwrap();
        let usage =
            Arc::new(crate::usage::UsageStore::open(temp.path().join("usage.sqlite3")).unwrap());
        let mut persistent = DecisionObserver::default();
        persistent.set_sink(Arc::new(
            crate::diagnostics_store::DesktopDiagnosticsSink::new(usage.clone()),
        ));
        for event in sink.0.lock().unwrap().iter() {
            let DiagnosticEvent::QuotaPoolDecision(d) = event else {
                panic!("wrong event")
            };
            persistent.record(d.clone());
            persistent.record(d.clone());
        }
        let (exported, truncated) = usage.diagnostic_log_for_support(16_384).unwrap();
        assert!(!truncated);
        assert_eq!(exported.lines().count(), 3);
        for line in exported.lines() {
            let event: DiagnosticEvent = serde_json::from_str(line).unwrap();
            let DiagnosticEvent::QuotaPoolDecision(d) = event else {
                panic!("wrong persisted kind")
            };
            assert_eq!(d.candidates[0].tokens_per_point, 10_000.0);
        }
    }
}
