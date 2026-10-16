//! Cooperative deployment waits. Consent is scoped to one live operation and
//! the exact observed App-owned process; no approval survives a process change.
use crate::error::{AppError, AppResult};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Default)]
struct Consent {
    fingerprint: String,
    token: String,
    approved: bool,
}

pub(crate) struct DeploymentControl {
    pub host_id: String,
    cancelled: AtomicBool,
    consent: Mutex<Option<Consent>>,
    changed: tokio::sync::Notify,
}

type Registry = Mutex<BTreeMap<String, Arc<DeploymentControl>>>;
fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

pub(crate) fn register(operation_id: &str, host_id: &str) -> AppResult<Arc<DeploymentControl>> {
    let mut active = registry().lock().unwrap();
    if active.values().any(|control| control.host_id == host_id) {
        return Err(AppError::Message("DeploymentAlreadyRunning".into()));
    }
    let control = Arc::new(DeploymentControl::new(host_id));
    active.insert(operation_id.into(), control.clone());
    Ok(control)
}
pub(crate) fn unregister(operation_id: &str) {
    registry().lock().unwrap().remove(operation_id);
}
pub(crate) struct Registration(pub String);
impl Drop for Registration {
    fn drop(&mut self) {
        unregister(&self.0);
    }
}
pub(crate) fn resolve(operation_id: &str, host_id: &str) -> AppResult<Arc<DeploymentControl>> {
    registry()
        .lock()
        .unwrap()
        .get(operation_id)
        .filter(|control| control.host_id == host_id)
        .cloned()
        .ok_or_else(|| AppError::Message("DeploymentControlUnavailable".into()))
}

impl DeploymentControl {
    fn new(host_id: &str) -> Self {
        Self {
            host_id: host_id.into(),
            cancelled: AtomicBool::new(false),
            consent: Mutex::new(None),
            changed: Default::default(),
        }
    }
    pub fn check(&self) -> AppResult<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(AppError::Message("DeploymentCancelled".into()))
        } else {
            Ok(())
        }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.clear_consent();
        self.changed.notify_one();
    }
    pub fn token(&self) -> Option<String> {
        self.consent
            .lock()
            .unwrap()
            .as_ref()
            .map(|consent| consent.token.clone())
    }
    fn clear_consent(&self) {
        *self.consent.lock().unwrap() = None;
    }
    fn offer(&self, fingerprint: &str) -> bool {
        let mut guard = self.consent.lock().unwrap();
        if guard
            .as_ref()
            .is_none_or(|consent| consent.fingerprint != fingerprint)
        {
            *guard = Some(Consent {
                fingerprint: fingerprint.into(),
                token: ulid::Ulid::new().to_string(),
                approved: false,
            });
        }
        guard.as_ref().is_some_and(|consent| consent.approved)
    }
    pub fn confirm(&self, token: &str) -> AppResult<()> {
        self.check()?;
        let mut guard = self.consent.lock().unwrap();
        let consent = guard
            .as_mut()
            .filter(|consent| consent.token == token)
            .ok_or_else(|| AppError::Message("DeploymentConfirmationExpired".into()))?;
        consent.approved = true;
        self.changed.notify_one();
        Ok(())
    }
    pub async fn wait(&self) {
        tokio::select! { _ = self.changed.notified() => {}, _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {} }
    }
}

pub(crate) trait WaitClient {
    fn observe(&self) -> AppResult<(Value, Option<Value>)>;
    fn stop_app_owned(&self, native: &Value) -> AppResult<()>;
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WaitStep {
    Ready,
    Observing,
    WaitingForIdle,
    AwaitingTakeover,
    TookOver,
}

/// A missing observation is bounded separately from legitimately active work.
#[derive(Default)]
pub(crate) struct ObservationDeadline {
    missing_since: Option<std::time::Duration>,
}
impl ObservationDeadline {
    pub fn check(&mut self, step: &WaitStep, elapsed: std::time::Duration) -> AppResult<()> {
        if *step != WaitStep::Observing {
            self.missing_since = None;
        } else {
            let since = *self.missing_since.get_or_insert(elapsed);
            if elapsed.saturating_sub(since) >= std::time::Duration::from_secs(30) {
                return Err(AppError::Message(
                    "incompleteObservation: unable to observe Codex daemon, turns, tools or approvals for 30 seconds".into(),
                ));
            }
        }
        Ok(())
    }
}

pub(crate) trait PreparationClient {
    type Output;
    fn prepare(&mut self) -> AppResult<Self::Output>;
    /// Must include a cancellable tick even when the observation is already ready.
    async fn wait_for_retry(&mut self) -> AppResult<()>;
}

pub(crate) async fn prepare_bounded<C: PreparationClient>(client: &mut C) -> AppResult<C::Output> {
    let mut previous_blocker = None;
    // At most three retries, even if inconsistent observers alternate blockers.
    for attempt in 0..=3 {
        let error = match client.prepare() {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let message = error.to_string();
        let blocker = [
            "activeWorkInProgress",
            "activeTurnInProgress",
            "nativeDaemonAppOwned",
            "incompleteObservation",
        ]
        .into_iter()
        .find(|code| message.contains(code));
        if blocker.is_none() || blocker == previous_blocker || attempt == 3 {
            return Err(error);
        }
        previous_blocker = blocker;
        client.wait_for_retry().await?;
    }
    unreachable!("bounded preparation returns on its last attempt")
}

fn fingerprint(native: &Value) -> Option<String> {
    let pid = native.get("daemonPid")?.as_u64().filter(|pid| *pid > 0)?;
    let identity = native
        .get("daemonIdentity")?
        .as_str()
        .filter(|identity| !identity.is_empty())?;
    let home = native
        .get("codexHome")?
        .as_str()
        .filter(|home| !home.is_empty())?;
    Some(format!("{pid}:{identity}:{home}"))
}

pub(crate) fn step(control: &DeploymentControl, client: &dyn WaitClient) -> AppResult<WaitStep> {
    control.check()?;
    let (status, sessions) = client.observe()?;
    control.check()?;
    let native = status.get("nativeCodex");
    if native.is_none_or(Value::is_null) {
        control.clear_consent();
        return Ok(WaitStep::Observing);
    }
    if native
        .and_then(|native| native.get("daemonRunning"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        control.clear_consent();
        return Ok(WaitStep::Ready);
    }
    if let Err(code) =
        super::observation::require_idle_for_destructive_op(sessions.as_ref(), native)
    {
        control.clear_consent();
        return Ok(if code == "incompleteObservation" {
            WaitStep::Observing
        } else {
            WaitStep::WaitingForIdle
        });
    }
    let native = native.ok_or_else(|| AppError::Message("incompleteObservation".into()))?;
    if native.get("restartSafe").and_then(Value::as_bool) == Some(true) {
        control.clear_consent();
        return Ok(WaitStep::Ready);
    }
    if native.get("daemonOwner").and_then(Value::as_str) != Some("codexAppDirect") {
        return Err(AppError::Message(
            "incompleteObservation: unknown daemon owner".into(),
        ));
    }
    let fingerprint = fingerprint(native).ok_or_else(|| {
        AppError::Message("incompleteObservation: daemon identity missing".into())
    })?;
    if !control.offer(&fingerprint) {
        return Ok(WaitStep::AwaitingTakeover);
    }
    // Consume before the mutation. Any refusal/race needs a fresh observation
    // and fresh consent; an approval never authorizes a replacement process.
    control.clear_consent();
    control.check()?;
    match client.stop_app_owned(native) {
        Ok(()) => Ok(WaitStep::TookOver),
        Err(error)
            if error.to_string().contains("activeTurnInProgress")
                || error.to_string().contains("IdentityChanged") =>
        {
            Ok(WaitStep::WaitingForIdle)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[tokio::test]
    async fn inconsistent_prepare_and_ready_observation_stop_after_one_waited_retry() {
        struct FakePreparation {
            attempts: usize,
            ticks: usize,
            alternating: bool,
        }
        impl PreparationClient for FakePreparation {
            type Output = ();
            fn prepare(&mut self) -> AppResult<()> {
                self.attempts += 1;
                let code = if self.alternating && self.attempts.is_multiple_of(2) {
                    "incompleteObservation"
                } else {
                    "nativeDaemonAppOwned"
                };
                Err(AppError::Message(format!(
                    "{code}: original prepare failure"
                )))
            }
            async fn wait_for_retry(&mut self) -> AppResult<()> {
                // Fake the cancellable tick without sleeping in unit tests.
                self.ticks += 1;
                let client = FakeClient::idle();
                client.status.borrow_mut()["nativeCodex"]["restartSafe"] = true.into();
                assert_eq!(
                    step(&DeploymentControl::new("host"), &client)?,
                    WaitStep::Ready
                );
                client.status.borrow_mut()["nativeCodex"]["daemonRunning"] = false.into();
                assert_eq!(
                    step(&DeploymentControl::new("host"), &client)?,
                    WaitStep::Ready
                );
                Ok(())
            }
        }
        let mut client = FakePreparation {
            attempts: 0,
            ticks: 0,
            alternating: false,
        };
        assert_eq!(
            prepare_bounded(&mut client).await.unwrap_err().to_string(),
            "nativeDaemonAppOwned: original prepare failure"
        );
        assert_eq!((client.attempts, client.ticks), (2, 1));
        client = FakePreparation {
            attempts: 0,
            ticks: 0,
            alternating: true,
        };
        assert_eq!(
            prepare_bounded(&mut client).await.unwrap_err().to_string(),
            "incompleteObservation: original prepare failure"
        );
        assert_eq!((client.attempts, client.ticks), (4, 3));
    }

    #[test]
    fn missing_native_state_times_out_and_recovered_observation_resets_deadline() {
        use std::time::Duration;
        for status in [
            serde_json::json!({}),
            serde_json::json!({"nativeCodex": null}),
        ] {
            let client = FakeClient::idle();
            *client.status.borrow_mut() = status;
            let control = DeploymentControl::new("host");
            let mut deadline = ObservationDeadline::default();
            for seconds in [0, 2, 28, 29] {
                let next = step(&control, &client).unwrap();
                assert_eq!(next, WaitStep::Observing);
                deadline.check(&next, Duration::from_secs(seconds)).unwrap();
            }
            let error = deadline
                .check(&step(&control, &client).unwrap(), Duration::from_secs(30))
                .unwrap_err()
                .to_string();
            assert!(error.contains("incompleteObservation"));
            assert!(error.contains("unable to observe Codex"));
            assert!(client.stops.borrow().is_empty());
            assert!(control.token().is_none());
            deadline
                .check(&WaitStep::WaitingForIdle, Duration::from_secs(31))
                .unwrap();
            deadline
                .check(&WaitStep::Observing, Duration::from_secs(60))
                .unwrap();
            deadline
                .check(&WaitStep::Observing, Duration::from_secs(89))
                .unwrap();
            assert!(deadline
                .check(&WaitStep::Observing, Duration::from_secs(90))
                .is_err());
            control.cancel();
            assert_eq!(
                step(&control, &client).unwrap_err().to_string(),
                "DeploymentCancelled"
            );
        }
    }

    #[tokio::test]
    async fn cancel_wakes_the_wait_without_waiting_for_another_tick() {
        let control = DeploymentControl::new("host");
        control.cancel();
        tokio::time::timeout(std::time::Duration::from_millis(100), control.wait())
            .await
            .unwrap();
        assert_eq!(
            control.check().unwrap_err().to_string(),
            "DeploymentCancelled"
        );
    }
    struct FakeClient {
        status: RefCell<Value>,
        sessions: RefCell<Option<Value>>,
        stops: RefCell<Vec<Value>>,
        reads: Cell<usize>,
        fail_stop: RefCell<Option<String>>,
    }
    impl FakeClient {
        fn idle() -> Self {
            Self {
                status: RefCell::new(
                    serde_json::json!({"nativeCodex": {"daemonRunning": true, "daemonOwner": "codexAppDirect", "restartSafe": false, "daemonPid": 42, "daemonIdentity": "boot:start:42", "codexHome": "/home/test/.codex"}}),
                ),
                sessions: RefCell::new(Some(
                    serde_json::json!({"observability": "nativeAppServer", "threads": [{"active": false, "activeTools": false, "pendingApprovals": false}]}),
                )),
                stops: Default::default(),
                reads: Cell::new(0),
                fail_stop: Default::default(),
            }
        }
    }
    impl WaitClient for FakeClient {
        fn observe(&self) -> AppResult<(Value, Option<Value>)> {
            self.reads.set(self.reads.get() + 1);
            Ok((self.status.borrow().clone(), self.sessions.borrow().clone()))
        }
        fn stop_app_owned(&self, native: &Value) -> AppResult<()> {
            if let Some(error) = self.fail_stop.borrow().as_ref() {
                return Err(AppError::Message(error.clone()));
            }
            self.stops.borrow_mut().push(native.clone());
            self.status.borrow_mut()["nativeCodex"]["daemonRunning"] = false.into();
            Ok(())
        }
    }
    #[test]
    fn active_turn_tools_approvals_and_incomplete_observation_never_offer_takeover() {
        for field in ["active", "activeTools", "pendingApprovals"] {
            let client = FakeClient::idle();
            client.sessions.borrow_mut().as_mut().unwrap()["threads"][0][field] = true.into();
            let control = DeploymentControl::new("host");
            assert_eq!(step(&control, &client).unwrap(), WaitStep::WaitingForIdle);
            assert!(control.token().is_none());
            assert!(client.stops.borrow().is_empty());
        }
        let client = FakeClient::idle();
        *client.sessions.borrow_mut() = None;
        let control = DeploymentControl::new("host");
        assert_eq!(step(&control, &client).unwrap(), WaitStep::Observing);
        assert!(client.stops.borrow().is_empty());
        *client.status.borrow_mut() = serde_json::json!({"nativeCodex": null});
        assert_eq!(step(&control, &client).unwrap(), WaitStep::Observing);
    }
    #[test]
    fn idle_takeover_needs_one_confirmation_and_passes_exact_identity_to_agent() {
        let client = FakeClient::idle();
        let control = DeploymentControl::new("host");
        assert_eq!(step(&control, &client).unwrap(), WaitStep::AwaitingTakeover);
        assert!(client.stops.borrow().is_empty());
        let token = control.token().unwrap();
        control.confirm(&token).unwrap();
        assert_eq!(step(&control, &client).unwrap(), WaitStep::TookOver);
        assert_eq!(client.stops.borrow()[0]["daemonPid"], 42);
        assert_eq!(client.stops.borrow()[0]["daemonIdentity"], "boot:start:42");
        assert_eq!(step(&control, &client).unwrap(), WaitStep::Ready);
        assert_eq!(client.stops.borrow().len(), 1);
        assert!(control.confirm(&token).is_err());
    }
    #[test]
    fn changed_pid_start_time_or_home_expires_old_consent() {
        for (key, value) in [
            ("daemonPid", serde_json::json!(43)),
            ("daemonIdentity", serde_json::json!("new-start")),
            ("codexHome", serde_json::json!("/different")),
        ] {
            let client = FakeClient::idle();
            let control = DeploymentControl::new("host");
            step(&control, &client).unwrap();
            let token = control.token().unwrap();
            control.confirm(&token).unwrap();
            client.status.borrow_mut()["nativeCodex"][key] = value;
            assert_eq!(step(&control, &client).unwrap(), WaitStep::AwaitingTakeover);
            assert_ne!(control.token().as_deref(), Some(token.as_str()));
            assert!(control.confirm(&token).is_err());
            assert!(client.stops.borrow().is_empty());
        }
    }
    #[test]
    fn renewed_activity_revokes_consent_and_cancellation_prevents_further_rpc() {
        let client = FakeClient::idle();
        let control = DeploymentControl::new("host");
        step(&control, &client).unwrap();
        control.confirm(&control.token().unwrap()).unwrap();
        client.sessions.borrow_mut().as_mut().unwrap()["threads"][0]["active"] = true.into();
        assert_eq!(step(&control, &client).unwrap(), WaitStep::WaitingForIdle);
        assert!(control.token().is_none());
        control.cancel();
        let reads = client.reads.get();
        assert_eq!(
            step(&control, &client).unwrap_err().to_string(),
            "DeploymentCancelled"
        );
        assert_eq!(client.reads.get(), reads);
        assert!(client.stops.borrow().is_empty());
    }
    #[test]
    fn agent_side_race_requires_new_confirmation_and_unknown_owner_is_never_stopped() {
        let client = FakeClient::idle();
        let control = DeploymentControl::new("host");
        step(&control, &client).unwrap();
        let token = control.token().unwrap();
        control.confirm(&token).unwrap();
        *client.fail_stop.borrow_mut() = Some("NativeAppOwnedStopIdentityChanged".into());
        assert_eq!(step(&control, &client).unwrap(), WaitStep::WaitingForIdle);
        assert!(control.confirm(&token).is_err());
        client.status.borrow_mut()["nativeCodex"]["daemonOwner"] = "unknown".into();
        assert!(step(&control, &client)
            .unwrap_err()
            .to_string()
            .contains("incompleteObservation"));
        assert!(client.stops.borrow().is_empty());
    }
    #[test]
    fn managed_daemon_continues_without_prompt_and_transport_errors_stay_errors() {
        let client = FakeClient::idle();
        let control = DeploymentControl::new("host");
        client.status.borrow_mut()["nativeCodex"]["restartSafe"] = true.into();
        assert_eq!(step(&control, &client).unwrap(), WaitStep::Ready);
        assert!(client.stops.borrow().is_empty());
        struct Failed;
        impl WaitClient for Failed {
            fn observe(&self) -> AppResult<(Value, Option<Value>)> {
                Err(AppError::Message("agent.transportTimeout".into()))
            }
            fn stop_app_owned(&self, _: &Value) -> AppResult<()> {
                panic!("transport failed")
            }
        }
        assert_eq!(
            step(&control, &Failed).unwrap_err().to_string(),
            "agent.transportTimeout"
        );
    }
    #[test]
    fn live_registry_binds_host_and_does_not_reuse_old_approvals() {
        let id = format!("test-{}", ulid::Ulid::new());
        let host = format!("test-host-{}", ulid::Ulid::new());
        let control = register(&id, &host).unwrap();
        let registration = Registration(id.clone());
        assert!(register("another", &host).is_err());
        assert!(resolve(&id, "another-host").is_err());
        assert!(resolve(&id, &host).is_ok());
        control.cancel();
        drop(registration);
        assert!(resolve(&id, &host).is_err());
    }
}
