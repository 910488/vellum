//! Delegation runtime: a Multi-Agent V2 equivalent for translated routes.
//!
//! Issue #6 Phase 8. Sol and Terra ship Codex's Multi-Agent V2; Luna is still
//! V1. Vellum cannot set `multi_agent_version` and inherit any of it — that
//! switches Codex's private delegation runtime, and a parent that can spawn
//! children Vellum cannot run is the same surface/runtime split as every other
//! spoofed flag.
//!
//! What Vellum *can* do is run children itself: it already makes its own
//! upstream requests for compaction, so a child agent is another conversation
//! against a provider it is already authenticated for. This module owns that
//! lifecycle — identity, spawn/message/wait, in-memory usage attribution, and failure —
//! as a runtime that can be verified before anything is advertised.
//!
//! **Off by default.** The issue gates Multi-Agent on single-agent Sol parity
//! passing first, so nothing here activates without an explicit opt-in:
//! [`DelegationRuntime::is_verified`] is false until set, no profile resolves
//! to the delegated policy unless asked, and [`reasoning_efforts_for`]
//! withholds `ultra` until it does — the issue is explicit that `ultra` must
//! not be offered without a verified delegation runtime, because it is
//! described as maximum reasoning *with automatic task delegation*.

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Namespace the delegation tools live under, mirroring how Codex groups its
/// own Multi-Agent V2 surface. Translated routes flatten this to
/// `multi_agent__spawn` via [`super::tools::flatten_namespace_name`].
pub const NAMESPACE: &str = "multi_agent";

/// Whether the delegation plumbing is connected and complete enough to expose.
///
/// Delegation is deliberately synchronous: the only advertised operation is
/// `spawn`, which returns the child result. Child models are restricted to the
/// parent's route, streaming parent turns are buffered and reconstructed for
/// Codex, and every provider turn is durably attributed to its parent or child.
pub const RUNTIME_WIRED: bool = true;

/// A parent may not spawn without bound, and a child may not spawn at all:
/// depth beyond one turns a delegation bug into an unbounded fan-out.
pub const MAX_CHILDREN: usize = 8;
pub const MAX_CONCURRENT_CHILDREN: usize = 4;
pub const MAX_DEPTH: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentRole {
    Parent,
    Child,
}

impl AgentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Child => "child",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentState {
    Spawned,
    Running,
    /// Finished its work and is holding a result the parent has not read.
    Completed,
    /// Ended without a usable result. Never silently reported as completed.
    Failed,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spawned => "spawned",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

/// Who an agent is. `model` is the catalog id the parent asked for and `
/// upstream_model` is what the provider was actually given — the two are
/// recorded separately so a substitution can never hide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIdentity {
    pub agent_id: String,
    pub role: AgentRole,
    pub model: String,
    pub upstream_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub depth: usize,
}

/// Tokens attributed to one agent. Child usage is never folded into the
/// parent's line: the whole point of attribution is that a delegated turn is
/// visibly a delegated turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl AgentUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    fn add(&mut self, other: AgentUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessage {
    pub from: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRecord {
    pub identity: AgentIdentity,
    pub state: AgentState,
    pub instructions: String,
    pub messages: Vec<AgentMessage>,
    pub usage: AgentUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// What the parent asked for when spawning.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnSpec {
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub instructions: String,
}

/// Models the runtime may actually route a child to.
#[derive(Debug, Clone, Default)]
pub struct AvailableModels {
    /// catalog id → upstream model.
    models: BTreeMap<String, String>,
    /// catalog id → reasoning efforts the model verifiably supports.
    efforts: BTreeMap<String, Vec<String>>,
}

impl AvailableModels {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_model(mut self, catalog_id: &str, upstream_model: &str, efforts: &[&str]) -> Self {
        self.models
            .insert(catalog_id.to_string(), upstream_model.to_string());
        self.efforts.insert(
            catalog_id.to_string(),
            efforts.iter().map(|effort| effort.to_string()).collect(),
        );
        self
    }

    pub fn upstream_for(&self, catalog_id: &str) -> Option<&str> {
        self.models.get(catalog_id).map(String::as_str)
    }

    pub fn supports_effort(&self, catalog_id: &str, effort: &str) -> bool {
        self.efforts
            .get(catalog_id)
            .is_some_and(|efforts| efforts.iter().any(|known| known == effort))
    }

    /// Models a child may use, restricted to the parent's own route.
    ///
    /// Delegation reuses the parent's endpoint, wire and credentials, so a
    /// model belonging to a *different* route cannot be honoured: sending it to
    /// the parent's provider with only the model name swapped would cross a
    /// provider, a dialect and an account realm at once. Until a child can
    /// carry its own route, anything off-route is simply not available, and
    /// [`DelegationRuntime::spawn`] refuses it by name rather than substituting.
    pub fn on_route(routes: &[crate::model::ModelRoute], route_id: &str) -> Self {
        let mut available = Self::new();
        for model in routes.iter().filter(|model| model.route_id == route_id) {
            available
                .models
                .insert(model.catalog_id.clone(), model.upstream_model.clone());
            available
                .efforts
                .insert(model.catalog_id.clone(), model.reasoning_efforts.clone());
        }
        available
    }

    pub fn known(&self) -> Vec<&str> {
        self.models.keys().map(String::as_str).collect()
    }
}

#[derive(Debug)]
pub struct DelegationRuntime {
    parent: AgentIdentity,
    available: AvailableModels,
    agents: BTreeMap<String, AgentRecord>,
    order: Vec<String>,
    next_id: usize,
    /// Whether this runtime has been proven end to end. Until it has, nothing
    /// about delegation may be advertised to a model.
    verified: bool,
}

impl DelegationRuntime {
    pub fn new(parent: AgentIdentity, available: AvailableModels) -> Self {
        Self {
            parent,
            available,
            agents: BTreeMap::new(),
            order: Vec::new(),
            next_id: 1,
            verified: false,
        }
    }

    /// Mark the runtime as proven. Separate from construction so enabling
    /// delegation is an explicit, reviewable act rather than a side effect of
    /// the code existing.
    pub fn mark_verified(&mut self) {
        self.verified = true;
    }

    pub fn is_verified(&self) -> bool {
        self.verified
    }

    pub fn parent(&self) -> &AgentIdentity {
        &self.parent
    }

    pub fn agent(&self, agent_id: &str) -> Option<&AgentRecord> {
        self.agents.get(agent_id)
    }

    pub fn children(&self) -> Vec<&AgentRecord> {
        self.order
            .iter()
            .filter_map(|id| self.agents.get(id))
            .collect()
    }

    fn active_children(&self) -> usize {
        self.agents
            .values()
            .filter(|agent| !agent.state.is_terminal())
            .count()
    }

    /// Start a child agent.
    ///
    /// Every rejection here is loud. In particular an unavailable model is an
    /// error rather than a substitution: a parent that asked for a strong model
    /// and silently received a weak one produces work that looks delegated and
    /// reviewed but is neither.
    pub fn spawn(&mut self, spec: SpawnSpec) -> AppResult<AgentIdentity> {
        if !self.verified {
            return Err(AppError::Message(
                "delegation runtime is not verified; multi-agent spawning is unavailable on this route".into(),
            ));
        }
        if self.parent.depth >= MAX_DEPTH {
            return Err(AppError::Message(format!(
                "agent `{}` is at depth {} and may not spawn children (maximum depth {MAX_DEPTH})",
                self.parent.agent_id, self.parent.depth
            )));
        }
        if self.order.len() >= MAX_CHILDREN {
            return Err(AppError::Message(format!(
                "already spawned {MAX_CHILDREN} children, which is the maximum for one parent turn"
            )));
        }
        if self.active_children() >= MAX_CONCURRENT_CHILDREN {
            return Err(AppError::Message(format!(
                "{MAX_CONCURRENT_CHILDREN} children are already running; wait for one before spawning another"
            )));
        }
        if spec.instructions.trim().is_empty() {
            return Err(AppError::Message(
                "a child agent needs instructions describing what it must deliver".into(),
            ));
        }
        let Some(upstream_model) = self.available.upstream_for(&spec.model) else {
            return Err(AppError::Message(format!(
                "model `{}` is not available for delegation on this route; available: {}. Refusing to substitute a different model.",
                spec.model,
                self.available.known().join(", ")
            )));
        };
        if let Some(effort) = spec.reasoning_effort.as_deref() {
            if !self.available.supports_effort(&spec.model, effort) {
                return Err(AppError::Message(format!(
                    "model `{}` has not been verified to support reasoning effort `{effort}`; refusing to downgrade silently",
                    spec.model
                )));
            }
        }

        let agent_id = format!("agent_{}", self.next_id);
        self.next_id += 1;
        let identity = AgentIdentity {
            agent_id: agent_id.clone(),
            role: AgentRole::Child,
            model: spec.model.clone(),
            upstream_model: upstream_model.to_string(),
            reasoning_effort: spec.reasoning_effort.clone(),
            parent_id: Some(self.parent.agent_id.clone()),
            depth: self.parent.depth + 1,
        };
        self.agents.insert(
            agent_id.clone(),
            AgentRecord {
                identity: identity.clone(),
                state: AgentState::Spawned,
                instructions: spec.instructions,
                messages: Vec::new(),
                usage: AgentUsage::default(),
                result: None,
                failure: None,
            },
        );
        self.order.push(agent_id);
        Ok(identity)
    }

    fn agent_mut(&mut self, agent_id: &str) -> AppResult<&mut AgentRecord> {
        self.agents
            .get_mut(agent_id)
            .ok_or_else(|| AppError::Message(format!("unknown agent `{agent_id}`")))
    }

    /// Send a message to a running child.
    pub fn message(&mut self, agent_id: &str, text: &str) -> AppResult<()> {
        let from = self.parent.agent_id.clone();
        let agent = self.agent_mut(agent_id)?;
        if agent.state.is_terminal() {
            return Err(AppError::Message(format!(
                "agent `{agent_id}` has already {}; it cannot receive messages",
                agent.state.as_str()
            )));
        }
        agent.state = AgentState::Running;
        agent.messages.push(AgentMessage {
            from,
            text: text.to_string(),
        });
        Ok(())
    }

    /// Record what a child produced, together with what it cost.
    pub fn complete(&mut self, agent_id: &str, result: &str, usage: AgentUsage) -> AppResult<()> {
        let agent = self.agent_mut(agent_id)?;
        if agent.state.is_terminal() {
            return Err(AppError::Message(format!(
                "agent `{agent_id}` is already {}",
                agent.state.as_str()
            )));
        }
        agent.state = AgentState::Completed;
        agent.result = Some(result.to_string());
        agent.usage.add(usage);
        Ok(())
    }

    /// Record a failure. A failed child never reports a result.
    pub fn fail(&mut self, agent_id: &str, reason: &str, usage: AgentUsage) -> AppResult<()> {
        let agent = self.agent_mut(agent_id)?;
        agent.state = AgentState::Failed;
        agent.failure = Some(reason.to_string());
        agent.result = None;
        agent.usage.add(usage);
        Ok(())
    }

    /// Read a child's outcome once it has one.
    ///
    /// A failure surfaces as an error rather than an empty result, so a parent
    /// cannot mistake "the child could not do it" for "the child found
    /// nothing".
    pub fn wait(&self, agent_id: &str) -> AppResult<&AgentRecord> {
        let agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| AppError::Message(format!("unknown agent `{agent_id}`")))?;
        match agent.state {
            AgentState::Completed => Ok(agent),
            AgentState::Failed => Err(AppError::Message(format!(
                "agent `{agent_id}` ({}) failed: {}",
                agent.identity.model,
                agent.failure.as_deref().unwrap_or("no reason recorded")
            ))),
            other => Err(AppError::Message(format!(
                "agent `{agent_id}` is still {}",
                other.as_str()
            ))),
        }
    }

    /// In-memory JSON usage split by agent. This is not the sqlite
    /// `harness_usage_attribution` table (that ledger is legacy-harness only).
    pub fn usage_attribution(&self, parent_usage: AgentUsage) -> Value {
        let children = self
            .order
            .iter()
            .filter_map(|id| self.agents.get(id))
            .map(|agent| {
                json!({
                    "agentId": agent.identity.agent_id,
                    "model": agent.identity.model,
                    "upstreamModel": agent.identity.upstream_model,
                    "reasoningEffort": agent.identity.reasoning_effort,
                    "state": agent.state.as_str(),
                    "inputTokens": agent.usage.input_tokens,
                    "outputTokens": agent.usage.output_tokens
                })
            })
            .collect::<Vec<_>>();
        let child_total: u64 = self
            .agents
            .values()
            .map(|agent| agent.usage.total())
            .fold(0, u64::saturating_add);
        json!({
            "parent": {
                "agentId": self.parent.agent_id,
                "model": self.parent.model,
                "inputTokens": parent_usage.input_tokens,
                "outputTokens": parent_usage.output_tokens
            },
            "children": children,
            "totals": {
                "parentTokens": parent_usage.total(),
                "childTokens": child_total,
                "allTokens": parent_usage.total().saturating_add(child_total)
            }
        })
    }

    /// What the parent can see about its children. Model and effort are always
    /// visible, so a delegated result can be judged against what produced it.
    pub fn visible_roster(&self) -> Value {
        json!(self
            .order
            .iter()
            .filter_map(|id| self.agents.get(id))
            .map(|agent| json!({
                "agentId": agent.identity.agent_id,
                "role": agent.identity.role.as_str(),
                "model": agent.identity.model,
                "reasoningEffort": agent.identity.reasoning_effort,
                "state": agent.state.as_str()
            }))
            .collect::<Vec<_>>())
    }
}

/// Flattened names the translated surface exposes.
pub fn tool_name(action: &str) -> String {
    super::tools::flatten_namespace_name(NAMESPACE, action)
}

/// Split a flattened delegation tool name back into its action.
pub fn parse_tool_name(name: &str) -> Option<&str> {
    name.strip_prefix(NAMESPACE)?.strip_prefix("__")
}

/// The provider request that runs one child turn.
///
/// A child is a scoped reasoning delegate: it receives its instructions and
/// returns text. It is given **no tools** — a child that could edit files would
/// need Codex's approval and sandbox path, which delegation does not have, and
/// inventing one would be the same overreach as spoofing a capability flag.
pub fn child_request(agent: &AgentRecord, stream: bool) -> Value {
    let mut input = vec![json!({
        "role": "user",
        "content": agent.instructions.clone()
    })];
    for message in &agent.messages {
        input.push(json!({"role": "user", "content": message.text.clone()}));
    }
    let mut body = json!({
        "model": agent.identity.upstream_model,
        "instructions": "You are a delegated sub-agent. Complete exactly the task described and reply with the result only. You have no tools: report what you conclude, and say plainly if the task cannot be done without running something.",
        "input": input,
        "stream": stream
    });
    if let Some(effort) = agent.identity.reasoning_effort.as_deref() {
        body["reasoning"] = json!({"effort": effort});
    }
    body
}

/// Pull a child's text result out of a provider response.
pub fn child_result(response: &Value) -> Option<String> {
    let text = response
        .get("output")
        .and_then(Value::as_array)?
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// Tokens a child turn consumed.
pub fn child_usage(response: &Value) -> AgentUsage {
    let usage = response.get("usage");
    AgentUsage {
        input_tokens: usage
            .and_then(|usage| usage.get("input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .and_then(|usage| usage.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

/// Parse a `multi_agent__spawn` call.
pub fn parse_spawn(arguments: &Value) -> Result<SpawnSpec, String> {
    let model = arguments
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .ok_or("`model` is required and must name a model explicitly")?;
    let instructions = arguments
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or("`instructions` is required and must describe what the child must deliver")?;
    Ok(SpawnSpec {
        model: model.to_string(),
        reasoning_effort: arguments
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .map(str::to_string),
        instructions: instructions.to_string(),
    })
}

/// The answer a `spawn` call receives once its child has run.
///
/// Delegation is synchronous: `spawn` runs the child and returns its result in
/// the same call. That is why there is no `message` or `wait` on the
/// model-visible surface — the child is already terminal by the time the parent
/// learns its id, so those two could never apply, and advertising them would be
/// a contract the runtime cannot honour.
///
/// `result` is the whole point of the call and is always present on success;
/// a failure carries `error` instead, never an empty result.
pub fn spawn_output(agent: &AgentRecord) -> Value {
    let mut answer = json!({
        "agentId": agent.identity.agent_id,
        "model": agent.identity.model,
        "upstreamModel": agent.identity.upstream_model,
        "reasoningEffort": agent.identity.reasoning_effort,
        "state": agent.state.as_str(),
        "inputTokens": agent.usage.input_tokens,
        "outputTokens": agent.usage.output_tokens
    });
    let object = answer.as_object_mut().expect("object literal");
    match (&agent.result, &agent.failure) {
        (Some(result), _) => {
            object.insert("result".into(), json!(result));
        }
        (None, Some(failure)) => {
            object.insert("error".into(), json!(failure));
        }
        (None, None) => {
            object.insert(
                "error".into(),
                json!("the child ended without a result or a recorded failure"),
            );
        }
    }
    answer
}

/// Whether delegation is actually available right now.
///
/// Both conditions must hold: the operator opted in *and* a runtime exists to
/// dispatch children. The opt-in alone is not enough — that is what let the
/// gate be bypassed before.
pub fn delegation_available() -> bool {
    RUNTIME_WIRED && super::harness_options_from_env().delegation_verified
}

// M3 (refactor(proxy-runtime): move harness contract to runtime): the pure
// delegation contracts now live in `vellum-proxy-runtime::harness::multi_agent`,
// shared with the headless daemon. Re-exported here so every existing
// `crate::harness::multi_agent::*` call site in this crate keeps resolving
// identically. The runtime never decides whether a delegation runtime is
// actually connected — that host fact stays in this module's
// [`delegation_available`].
pub use vellum_proxy_runtime::harness::multi_agent::{
    delegation_namespace, gate_reasoning_levels, is_delegating_effort, reasoning_efforts_for,
    strongest_permitted_effort,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn parent() -> AgentIdentity {
        AgentIdentity {
            agent_id: "agent_root".into(),
            role: AgentRole::Parent,
            model: "vlm-grok".into(),
            upstream_model: "grok-4.5".into(),
            reasoning_effort: Some("high".into()),
            parent_id: None,
            depth: 0,
        }
    }

    fn available() -> AvailableModels {
        AvailableModels::new()
            .with_model("vlm-grok", "grok-4.5", &["low", "high"])
            .with_model("vlm-grok-fast", "grok-4.5-fast", &["low"])
    }

    fn verified_runtime() -> DelegationRuntime {
        let mut runtime = DelegationRuntime::new(parent(), available());
        runtime.mark_verified();
        runtime
    }

    fn spec(model: &str) -> SpawnSpec {
        SpawnSpec {
            model: model.into(),
            reasoning_effort: None,
            instructions: "summarise the locale files".into(),
        }
    }

    #[test]
    fn an_unverified_runtime_refuses_to_spawn_and_advertises_nothing() {
        let mut runtime = DelegationRuntime::new(parent(), available());
        assert!(!runtime.is_verified());
        let error = runtime.spawn(spec("vlm-grok")).unwrap_err().to_string();
        assert!(error.contains("not verified"), "{error}");
        assert!(delegation_namespace(false).is_none());
        assert!(delegation_namespace(true).is_some());
    }

    #[test]
    fn ultra_is_withheld_until_delegation_is_verified() {
        let supported = ["low", "medium", "high", "ultra"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            reasoning_efforts_for(&supported, false),
            vec!["low", "medium", "high"]
        );
        assert_eq!(reasoning_efforts_for(&supported, true), supported);
    }

    /// The gate must withdraw `ultra` without disturbing anything else. An
    /// earlier version substituted `permitted.last()` whenever the provider
    /// gave no default, which changed the effort every third-party model
    /// without an explicit default would have run at.
    #[test]
    fn gating_ultra_leaves_every_other_default_exactly_as_reported() {
        let levels = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };

        // No provider default stays no default: the catalog keeps its own
        // first-level fallback rather than receiving a synthesized one.
        let (permitted, default) = gate_reasoning_levels(&levels(&["high", "medium"]), None, false);
        assert_eq!(permitted, vec!["high", "medium"]);
        assert_eq!(default, None);
        let (_, default) = gate_reasoning_levels(&levels(&["low", "medium", "high"]), None, false);
        assert_eq!(default, None);

        // A permitted default is passed through untouched.
        let (_, default) = gate_reasoning_levels(
            &levels(&["low", "medium", "high"]),
            Some("medium".into()),
            false,
        );
        assert_eq!(default.as_deref(), Some("medium"));

        // Only `ultra` is remapped, and to the strongest permitted level by
        // rank — not by where the provider happened to put it in the array.
        for order in [
            levels(&["low", "medium", "high", "ultra"]),
            levels(&["ultra", "high", "medium", "low"]),
        ] {
            let (permitted, default) = gate_reasoning_levels(&order, Some("ultra".into()), false);
            assert!(!permitted.iter().any(|effort| effort == "ultra"));
            assert_eq!(default.as_deref(), Some("high"), "order {order:?}");
        }

        // Once delegation is verified, nothing is withdrawn or rewritten.
        let (permitted, default) =
            gate_reasoning_levels(&levels(&["low", "ultra"]), Some("ultra".into()), true);
        assert_eq!(permitted, vec!["low", "ultra"]);
        assert_eq!(default.as_deref(), Some("ultra"));
    }

    #[test]
    fn strength_is_ranked_not_positional_and_ignores_unknown_names() {
        assert_eq!(
            strongest_permitted_effort(&["low".into(), "xhigh".into(), "medium".into()]),
            Some("xhigh".into())
        );
        // An unrecognised level has no known strength, so it is never chosen
        // over one that does.
        assert_eq!(
            strongest_permitted_effort(&["low".into(), "turbo".into()]),
            Some("low".into())
        );
        assert_eq!(strongest_permitted_effort(&["turbo".into()]), None);
        assert_eq!(strongest_permitted_effort(&[]), None);
    }

    #[test]
    fn a_child_carries_its_own_identity_model_and_effort() {
        let mut runtime = verified_runtime();
        let identity = runtime
            .spawn(SpawnSpec {
                model: "vlm-grok-fast".into(),
                reasoning_effort: Some("low".into()),
                instructions: "scan the screens".into(),
            })
            .unwrap();
        assert_eq!(identity.role, AgentRole::Child);
        assert_eq!(identity.model, "vlm-grok-fast");
        assert_eq!(identity.upstream_model, "grok-4.5-fast");
        assert_eq!(identity.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(identity.parent_id.as_deref(), Some("agent_root"));
        assert_eq!(identity.depth, 1);
        // The roster the parent sees names the model and effort behind a result.
        let roster = runtime.visible_roster();
        assert_eq!(roster[0]["model"], "vlm-grok-fast");
        assert_eq!(roster[0]["reasoningEffort"], "low");
        assert_eq!(roster[0]["state"], "spawned");
    }

    #[test]
    fn an_unavailable_model_fails_loudly_instead_of_being_substituted() {
        let mut runtime = verified_runtime();
        let error = runtime.spawn(spec("gpt-5.6-sol")).unwrap_err().to_string();
        assert!(error.contains("not available for delegation"), "{error}");
        assert!(error.contains("Refusing to substitute"), "{error}");
        assert!(runtime.children().is_empty());

        // An unverified effort is refused rather than quietly downgraded.
        let error = runtime
            .spawn(SpawnSpec {
                model: "vlm-grok-fast".into(),
                reasoning_effort: Some("high".into()),
                instructions: "x".into(),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("refusing to downgrade silently"), "{error}");
    }

    #[test]
    fn the_spawn_message_wait_lifecycle_runs_in_order() {
        let mut runtime = verified_runtime();
        let child = runtime.spawn(spec("vlm-grok")).unwrap();
        let id = child.agent_id.as_str();

        // Not finished yet, so waiting is an error rather than an empty result.
        assert!(runtime.wait(id).unwrap_err().to_string().contains("still"));

        runtime.message(id, "focus on zh-TW first").unwrap();
        assert_eq!(runtime.agent(id).unwrap().state, AgentState::Running);
        assert_eq!(runtime.agent(id).unwrap().messages[0].from, "agent_root");

        runtime
            .complete(
                id,
                "12 keys need translation",
                AgentUsage {
                    input_tokens: 900,
                    output_tokens: 120,
                },
            )
            .unwrap();
        let done = runtime.wait(id).unwrap();
        assert_eq!(done.result.as_deref(), Some("12 keys need translation"));
        assert_eq!(done.state, AgentState::Completed);

        // A terminal agent takes no further input and cannot be completed twice.
        assert!(runtime.message(id, "more").is_err());
        assert!(runtime
            .complete(id, "again", AgentUsage::default())
            .is_err());
    }

    #[test]
    fn a_failed_child_surfaces_as_an_error_not_an_empty_result() {
        let mut runtime = verified_runtime();
        let child = runtime.spawn(spec("vlm-grok")).unwrap();
        runtime
            .fail(
                &child.agent_id,
                "upstream returned 429",
                AgentUsage {
                    input_tokens: 40,
                    output_tokens: 0,
                },
            )
            .unwrap();
        let error = runtime.wait(&child.agent_id).unwrap_err().to_string();
        assert!(error.contains("failed: upstream returned 429"), "{error}");
        assert!(error.contains("vlm-grok"));
        assert!(runtime.agent(&child.agent_id).unwrap().result.is_none());
        // A failed child still costs tokens, and they are still attributed.
        assert_eq!(runtime.agent(&child.agent_id).unwrap().usage.total(), 40);
    }

    #[test]
    fn usage_is_attributed_per_agent_and_never_folded_into_the_parent() {
        let mut runtime = verified_runtime();
        let first = runtime.spawn(spec("vlm-grok")).unwrap();
        let second = runtime.spawn(spec("vlm-grok-fast")).unwrap();
        runtime
            .complete(
                &first.agent_id,
                "a",
                AgentUsage {
                    input_tokens: 100,
                    output_tokens: 20,
                },
            )
            .unwrap();
        runtime
            .complete(
                &second.agent_id,
                "b",
                AgentUsage {
                    input_tokens: 300,
                    output_tokens: 80,
                },
            )
            .unwrap();

        let attribution = runtime.usage_attribution(AgentUsage {
            input_tokens: 1_000,
            output_tokens: 200,
        });
        assert_eq!(attribution["parent"]["inputTokens"], 1_000);
        assert_eq!(attribution["totals"]["parentTokens"], 1_200);
        assert_eq!(attribution["totals"]["childTokens"], 500);
        assert_eq!(attribution["totals"]["allTokens"], 1_700);
        assert_eq!(attribution["children"][0]["model"], "vlm-grok");
        assert_eq!(attribution["children"][1]["upstreamModel"], "grok-4.5-fast");
    }

    #[test]
    fn fan_out_is_bounded_by_count_concurrency_and_depth() {
        let mut runtime = verified_runtime();
        for index in 0..MAX_CONCURRENT_CHILDREN {
            runtime
                .spawn(spec("vlm-grok"))
                .unwrap_or_else(|error| panic!("spawn {index} failed: {error}"));
        }
        let error = runtime.spawn(spec("vlm-grok")).unwrap_err().to_string();
        assert!(error.contains("already running"), "{error}");

        // Finishing one frees a slot.
        runtime
            .complete("agent_1", "done", AgentUsage::default())
            .unwrap();
        runtime.spawn(spec("vlm-grok")).unwrap();

        // A child may not itself delegate.
        let child_identity = AgentIdentity {
            depth: 1,
            role: AgentRole::Child,
            ..parent()
        };
        let mut nested = DelegationRuntime::new(child_identity, available());
        nested.mark_verified();
        assert!(nested
            .spawn(spec("vlm-grok"))
            .unwrap_err()
            .to_string()
            .contains("maximum depth"));
    }

    #[test]
    fn total_children_are_capped_even_as_slots_free_up() {
        let mut runtime = verified_runtime();
        for index in 0..MAX_CHILDREN {
            runtime.spawn(spec("vlm-grok")).unwrap();
            runtime
                .complete(&format!("agent_{}", index + 1), "x", AgentUsage::default())
                .unwrap();
        }
        assert!(runtime
            .spawn(spec("vlm-grok"))
            .unwrap_err()
            .to_string()
            .contains("maximum for one parent turn"));
    }

    #[test]
    fn a_child_needs_instructions() {
        let mut runtime = verified_runtime();
        let error = runtime
            .spawn(SpawnSpec {
                model: "vlm-grok".into(),
                reasoning_effort: None,
                instructions: "   ".into(),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("needs instructions"), "{error}");
    }

    #[test]
    fn a_child_turn_is_a_scoped_request_with_no_tools() {
        let mut runtime = verified_runtime();
        let child = runtime
            .spawn(SpawnSpec {
                model: "vlm-grok-fast".into(),
                reasoning_effort: Some("low".into()),
                instructions: "list the untranslated keys".into(),
            })
            .unwrap();
        runtime.message(&child.agent_id, "zh-TW only").unwrap();
        let request = child_request(runtime.agent(&child.agent_id).unwrap(), false);

        assert_eq!(request["model"], "grok-4.5-fast");
        assert_eq!(request["reasoning"]["effort"], "low");
        // A child gets no tools: it has no path to Codex's approval or sandbox,
        // so giving it one would be inventing a capability.
        assert!(request.get("tools").is_none());
        let input = request["input"].as_array().unwrap();
        assert_eq!(input[0]["content"], "list the untranslated keys");
        assert_eq!(input[1]["content"], "zh-TW only");
    }

    #[test]
    fn a_child_result_and_its_cost_are_read_back_from_the_provider() {
        let response = json!({
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "content": [{"type": "output_text", "text": "12 keys"}]}
            ],
            "usage": {"input_tokens": 800, "output_tokens": 40}
        });
        assert_eq!(child_result(&response).as_deref(), Some("12 keys"));
        assert_eq!(
            child_usage(&response),
            AgentUsage {
                input_tokens: 800,
                output_tokens: 40
            }
        );
        // A response with no message is not a result; the caller must fail the
        // child rather than report an empty success.
        assert_eq!(child_result(&json!({"output": []})), None);
    }

    #[test]
    fn spawn_arguments_must_name_a_model_and_a_task() {
        assert!(parse_spawn(&json!({"instructions": "x"}))
            .unwrap_err()
            .contains("`model` is required"));
        assert!(parse_spawn(&json!({"model": "m"}))
            .unwrap_err()
            .contains("`instructions` is required"));
        assert!(parse_spawn(&json!({"model": "m", "instructions": "  "}))
            .unwrap_err()
            .contains("`instructions` is required"));
        let spec = parse_spawn(&json!({
            "model": "vlm-grok",
            "instructions": "review",
            "reasoning_effort": "high"
        }))
        .unwrap();
        assert_eq!(spec.model, "vlm-grok");
        assert_eq!(spec.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn flattened_tool_names_round_trip() {
        assert_eq!(tool_name("spawn"), "multi_agent__spawn");
        assert_eq!(parse_tool_name("multi_agent__wait"), Some("wait"));
        assert_eq!(parse_tool_name("shell"), None);
        assert_eq!(parse_tool_name("multi_agentspawn"), None);
    }

    #[test]
    fn a_spawn_answer_carries_the_child_result_or_an_error_never_neither() {
        let mut runtime = verified_runtime();
        let done = runtime.spawn(spec("vlm-grok")).unwrap();
        runtime
            .complete(
                &done.agent_id,
                "12 keys are untranslated",
                AgentUsage::default(),
            )
            .unwrap();
        let answer = spawn_output(runtime.agent(&done.agent_id).unwrap());
        assert_eq!(answer["result"], "12 keys are untranslated");
        assert!(answer.get("error").is_none());

        let broken = runtime.spawn(spec("vlm-grok")).unwrap();
        runtime
            .fail(&broken.agent_id, "upstream 429", AgentUsage::default())
            .unwrap();
        let answer = spawn_output(runtime.agent(&broken.agent_id).unwrap());
        assert_eq!(answer["error"], "upstream 429");
        assert!(answer.get("result").is_none());
        assert_eq!(answer["state"], "failed");
    }

    #[test]
    fn a_child_may_only_use_a_model_on_the_parents_own_route() {
        let route_model = |route: &str, catalog: &str, upstream: &str| crate::model::ModelRoute {
            catalog_id: catalog.into(),
            display_name: catalog.into(),
            route_id: route.into(),
            upstream_model: upstream.into(),
            context_window: None,
            wire: crate::model::WireFormat::Responses,
            reasoning: false,
            streaming: false,
            vision: false,
            reasoning_efforts: vec!["low".into()],
            default_reasoning_effort: None,
            reasoning_effort_transport: Default::default(),
        };
        let routes = [
            route_model("route-a", "vlm-a", "grok-a"),
            route_model("route-b", "vlm-b", "other-b"),
        ];
        let available = AvailableModels::on_route(&routes, "route-a");
        assert_eq!(available.known(), vec!["vlm-a"]);

        let mut runtime = DelegationRuntime::new(parent(), available);
        runtime.mark_verified();
        // An off-route model is refused by name. Sending it to the parent's
        // endpoint with the model swapped would cross provider, wire and realm.
        let error = runtime.spawn(spec("vlm-b")).unwrap_err().to_string();
        assert!(error.contains("not available for delegation"), "{error}");
        assert!(error.contains("Refusing to substitute"));
        runtime.spawn(spec("vlm-a")).unwrap();
    }

    #[test]
    fn a_spawn_answer_names_the_model_and_effort_that_produced_it() {
        let mut runtime = verified_runtime();
        let child = runtime.spawn(spec("vlm-grok-fast")).unwrap();
        runtime
            .complete(
                &child.agent_id,
                "done",
                AgentUsage {
                    input_tokens: 10,
                    output_tokens: 2,
                },
            )
            .unwrap();
        let answer = spawn_output(runtime.agent(&child.agent_id).unwrap());
        assert_eq!(answer["model"], "vlm-grok-fast");
        assert_eq!(answer["upstreamModel"], "grok-4.5-fast");
        assert_eq!(answer["state"], "completed");
        assert_eq!(answer["inputTokens"], 10);
    }

    #[test]
    fn the_delegation_surface_is_namespaced_and_closed() {
        let namespace = delegation_namespace(true).unwrap();
        assert_eq!(namespace["type"], "namespace");
        assert_eq!(namespace["name"], NAMESPACE);
        let tools = namespace["tools"].as_array().unwrap();
        // Only `spawn`. Delegation is synchronous, so a child is terminal by
        // the time the parent has its id — `message` and `wait` could never
        // apply, and advertising them would be a contract with no runtime.
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "spawn");
        assert_eq!(tools[0]["parameters"]["additionalProperties"], false);
        assert!(tools[0]["description"]
            .as_str()
            .unwrap()
            .contains("no tools"));
        // Translated routes flatten the namespace but keep it in the name.
        assert_eq!(
            super::super::tools::flatten_namespace_name(NAMESPACE, "spawn"),
            "multi_agent__spawn"
        );
    }
}
