//! Codex Subagent Graph Registry and Execution Coordination.
//!
//! Maintains normalized parent-child thread relationships, execution ID to thread
//! bindings, cycle-safe traversals for cancellation fanout, and TTL-bounded capacity.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::codex_metadata::{
    CodexIdentitySource, CodexIdentityTrust, CodexOpaqueId, CodexTurnIdentity,
};
use crate::diagnostics::{LinkConfidence, SubagentLinkMethod};

pub const DEFAULT_MAX_GRAPH_NODES: usize = 4096;
pub const DEFAULT_GRAPH_TTL_MS: u64 = 3600 * 1000;
pub const MAX_TRAVERSAL_NODES: usize = 1024;

/// Durable logical Codex agent thread key (`session_id` + `thread_id`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CodexThreadKey {
    pub session_id: CodexOpaqueId,
    pub thread_id: CodexOpaqueId,
}

impl CodexThreadKey {
    pub fn new(session_id: CodexOpaqueId, thread_id: CodexOpaqueId) -> Self {
        Self {
            session_id,
            thread_id,
        }
    }

    pub fn formatted(&self) -> String {
        format!("{}:{}", self.session_id.as_str(), self.thread_id.as_str())
    }
}

impl std::fmt::Display for CodexThreadKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.session_id, self.thread_id)
    }
}

/// Logical Codex turn key (`thread` + `turn_id`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CodexTurnKey {
    pub thread: CodexThreadKey,
    pub turn_id: CodexOpaqueId,
}

impl CodexTurnKey {
    pub fn new(thread: CodexThreadKey, turn_id: CodexOpaqueId) -> Self {
        Self { thread, turn_id }
    }
}

/// A node in the subagent thread hierarchy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentThreadNode {
    pub key: CodexThreadKey,
    pub parent: Option<CodexThreadKey>,
    pub parent_turn_id: Option<CodexOpaqueId>,
    pub root_turn_id: Option<CodexOpaqueId>,
    pub forked_from_thread_id: Option<CodexOpaqueId>,

    pub agent_name: Option<String>,
    pub subagent_kind: Option<String>,

    pub latest_context_window_id: Option<CodexOpaqueId>,

    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub terminal: bool,
}

/// Binding of an internal execution ID to a Codex thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadExecutionBinding {
    pub execution_id: String,
    pub request_id: String,
    pub thread: CodexThreadKey,
    pub turn_id: Option<CodexOpaqueId>,
    pub context_window_id: Option<CodexOpaqueId>,
    pub connection_id: Option<String>,
    pub route_id: String,
    pub model: String,
    pub started_at_ms: u64,
}

/// Source of a spawn tool call binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnBindingSource {
    AppServerActivity,
    ExactDataPlaneCorrelation,
    LegacyHeuristic,
}

/// Status emitted by Codex app-server subagent activity events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexSubagentActivityStatus {
    Started,
    Running,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

/// Subagent activity signal from Codex control-plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexSubagentActivity {
    pub session_id: CodexOpaqueId,
    pub parent_thread_id: CodexOpaqueId,
    pub call_id: String,
    pub child_thread_id: CodexOpaqueId,
    pub agent_path: Option<String>,
    pub status: CodexSubagentActivityStatus,
}

/// Spawn tool call binding (`call_id -> child thread`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnCallBinding {
    pub parent: CodexThreadKey,
    pub call_id: String,
    pub child: CodexThreadKey,
    pub observed_at_ms: u64,
    pub source: SpawnBindingSource,
}

/// Result of binding spawn activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnBindingResult {
    pub inserted: bool,
    pub duplicate: bool,
    pub conflict: bool,
}

/// Observation payload for an individual turn.
#[derive(Debug, Clone)]
pub struct TurnObservation {
    pub execution_id: String,
    pub request_id: String,
    pub connection_id: Option<String>,
    pub route_id: String,
    pub model: String,
    pub observed_at_ms: u64,
}

/// Outcome of observing a turn in the graph registry.
#[derive(Debug, Clone)]
pub struct GraphObservation {
    pub thread: Option<CodexThreadKey>,
    pub parent: Option<CodexThreadKey>,
    pub edge_created: bool,
    pub execution_bound: bool,
    pub link_method: SubagentLinkMethod,
    pub confidence: LinkConfidence,
}

/// Statistics from graph pruning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneStats {
    pub nodes_pruned: usize,
    pub executions_pruned: usize,
    pub bindings_pruned: usize,
}

/// Errors raised during graph manipulation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SubagentGraphError {
    #[error("Conflicting parent for child thread {child}")]
    ConflictingParent { child: String },
    #[error("Self-parent edge rejected for thread {thread}")]
    SelfParent { thread: String },
    #[error("Spawn binding conflict for call_id {call_id}")]
    SpawnBindingConflict { call_id: String },
    #[error("Cycle detected in subagent thread hierarchy")]
    CycleDetected,
    #[error("Traversal limit exceeded: {0} nodes")]
    TraversalLimitExceeded(usize),
    #[error("Capacity exceeded")]
    CapacityExceeded,
}

/// In-memory registry of Codex subagent threads, executions, and hierarchy edges.
#[derive(Debug, Default)]
pub struct SubagentGraphRegistry {
    nodes: HashMap<CodexThreadKey, SubagentThreadNode>,
    executions: HashMap<String, ThreadExecutionBinding>,
    thread_executions: HashMap<CodexThreadKey, HashSet<String>>,
    turn_executions: HashMap<CodexTurnKey, HashSet<String>>,
    children: HashMap<CodexThreadKey, HashSet<CodexThreadKey>>,
    spawn_bindings: HashMap<String, SpawnCallBinding>,
    execution_terminal: HashMap<String, u64>,
}

impl SubagentGraphRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe a turn and integrate its structured identity into the agent graph.
    pub fn observe_turn(
        &mut self,
        identity: &CodexTurnIdentity,
        observation: TurnObservation,
    ) -> Result<GraphObservation, SubagentGraphError> {
        // 1. If identity is conflicted, fail-closed for graph mutation
        if identity.trust == CodexIdentityTrust::Conflict {
            return Ok(GraphObservation {
                thread: None,
                parent: None,
                edge_created: false,
                execution_bound: false,
                link_method: SubagentLinkMethod::None,
                confidence: LinkConfidence::None,
            });
        }

        // 2. Extract current thread key
        let (Some(session_id), Some(thread_id)) = (&identity.session_id, &identity.thread_id)
        else {
            return Ok(GraphObservation {
                thread: None,
                parent: None,
                edge_created: false,
                execution_bound: false,
                link_method: SubagentLinkMethod::None,
                confidence: LinkConfidence::None,
            });
        };

        let current_key = CodexThreadKey::new(session_id.clone(), thread_id.clone());

        // 3. Create or update thread node idempotently
        let now = observation.observed_at_ms;
        let mut parent_key = None;
        let mut edge_created = false;

        if let Some(parent_id) = &identity.parent_thread_id {
            let p_key = CodexThreadKey::new(session_id.clone(), parent_id.clone());
            if p_key == current_key {
                return Err(SubagentGraphError::SelfParent {
                    thread: current_key.formatted(),
                });
            }

            // Insertion-time DAG Cycle Check: Check if current_key is already an ancestor of p_key!
            let mut check_ancestor = Some(&p_key);
            let mut visited_ancestors = HashSet::new();
            visited_ancestors.insert(p_key.clone());
            while let Some(curr) = check_ancestor {
                if curr == &current_key {
                    return Err(SubagentGraphError::CycleDetected);
                }
                if let Some(node) = self.nodes.get(curr) {
                    if let Some(next_p) = &node.parent {
                        if !visited_ancestors.insert(next_p.clone()) {
                            return Err(SubagentGraphError::CycleDetected);
                        }
                        check_ancestor = Some(next_p);
                    } else {
                        check_ancestor = None;
                    }
                } else {
                    check_ancestor = None;
                }
            }

            // Check if node already exists with conflicting parent
            if let Some(existing_node) = self.nodes.get(&current_key) {
                if let Some(existing_p) = &existing_node.parent {
                    if existing_p != &p_key {
                        return Err(SubagentGraphError::ConflictingParent {
                            child: current_key.formatted(),
                        });
                    }
                }
            }

            // Ensure parent node exists
            self.nodes
                .entry(p_key.clone())
                .or_insert_with(|| SubagentThreadNode {
                    key: p_key.clone(),
                    parent: None,
                    parent_turn_id: None,
                    root_turn_id: None,
                    forked_from_thread_id: None,
                    agent_name: None,
                    subagent_kind: None,
                    latest_context_window_id: None,
                    first_seen_ms: now,
                    last_seen_ms: now,
                    terminal: false,
                });

            // Create parent->child edge
            let child_set = self.children.entry(p_key.clone()).or_default();
            if child_set.insert(current_key.clone()) {
                edge_created = true;
            }
            parent_key = Some(p_key);
        }

        // Insert or update current node
        let node = self
            .nodes
            .entry(current_key.clone())
            .or_insert_with(|| SubagentThreadNode {
                key: current_key.clone(),
                parent: parent_key.clone(),
                parent_turn_id: identity.parent_turn_id.clone(),
                root_turn_id: identity.root_turn_id.clone(),
                forked_from_thread_id: identity.forked_from_thread_id.clone(),
                agent_name: identity.agent_name.clone(),
                subagent_kind: identity.subagent_kind.clone(),
                latest_context_window_id: identity.context_window_id.clone(),
                first_seen_ms: now,
                last_seen_ms: now,
                terminal: false,
            });

        node.last_seen_ms = now;
        if parent_key.is_some() && node.parent.is_none() {
            node.parent = parent_key.clone();
        }
        if identity.parent_turn_id.is_some() && node.parent_turn_id.is_none() {
            node.parent_turn_id = identity.parent_turn_id.clone();
        }
        if identity.root_turn_id.is_some() && node.root_turn_id.is_none() {
            node.root_turn_id = identity.root_turn_id.clone();
        }
        if identity.context_window_id.is_some() {
            node.latest_context_window_id = identity.context_window_id.clone();
        }
        if identity.agent_name.is_some() {
            node.agent_name = identity.agent_name.clone();
        }
        if identity.subagent_kind.is_some() {
            node.subagent_kind = identity.subagent_kind.clone();
        }

        // 4. Bind execution ID to thread and turn
        let exec_binding = ThreadExecutionBinding {
            execution_id: observation.execution_id.clone(),
            request_id: observation.request_id.clone(),
            thread: current_key.clone(),
            turn_id: identity.turn_id.clone(),
            context_window_id: identity.context_window_id.clone(),
            connection_id: observation.connection_id,
            route_id: observation.route_id,
            model: observation.model,
            started_at_ms: now,
        };

        self.executions
            .insert(observation.execution_id.clone(), exec_binding);
        self.thread_executions
            .entry(current_key.clone())
            .or_default()
            .insert(observation.execution_id.clone());

        if let Some(turn_id) = &identity.turn_id {
            let turn_key = CodexTurnKey::new(current_key.clone(), turn_id.clone());
            self.turn_executions
                .entry(turn_key)
                .or_default()
                .insert(observation.execution_id);
        }

        let link_method = match identity.source {
            CodexIdentitySource::TurnMetadataHeader
            | CodexIdentitySource::CanonicalClientMetadata => {
                SubagentLinkMethod::OfficialThreadMetadata
            }
            CodexIdentitySource::FlatCompatibilityHeaders => {
                SubagentLinkMethod::CompatibilityHeader
            }
            _ => SubagentLinkMethod::OfficialThreadMetadata,
        };

        let confidence = match identity.trust {
            CodexIdentityTrust::Exact | CodexIdentityTrust::Structured => LinkConfidence::High,
            CodexIdentityTrust::Partial => LinkConfidence::Medium,
            _ => LinkConfidence::None,
        };

        Ok(GraphObservation {
            thread: Some(current_key),
            parent: parent_key,
            edge_created,
            execution_bound: true,
            link_method,
            confidence,
        })
    }

    /// Resolve active parent execution IDs for a child turn identity.
    /// Prioritizes exact match on `(parent_thread, parent_turn_id)`.
    /// If `parent_turn_id` is missing or not active, falls back to active executions on `parent_thread`.
    pub fn resolve_parent_executions(
        &self,
        session_id: &CodexOpaqueId,
        parent_thread_id: &CodexOpaqueId,
        parent_turn_id: Option<&CodexOpaqueId>,
    ) -> (Vec<String>, LinkConfidence) {
        let parent_thread_key = CodexThreadKey::new(session_id.clone(), parent_thread_id.clone());

        if let Some(turn_id) = parent_turn_id {
            let turn_key = CodexTurnKey::new(parent_thread_key.clone(), turn_id.clone());
            if let Some(execs) = self.turn_executions.get(&turn_key) {
                let active: Vec<String> = execs
                    .iter()
                    .filter(|e| !self.execution_terminal.contains_key(*e))
                    .cloned()
                    .collect();
                if !active.is_empty() {
                    return (active, LinkConfidence::High);
                }
            }
            // Explicit parent_turn_id was specified but not found or not active: fail-closed!
            return (Vec::new(), LinkConfidence::None);
        }

        // Only when parent_turn_id is None (legacy compatibility protocol)
        let active = self.active_executions_for_thread(&parent_thread_key);
        if active.len() == 1 {
            (active, LinkConfidence::Medium)
        } else {
            // 0 or >1 active executions is ambiguous without a turn_id: fail-closed!
            (Vec::new(), LinkConfidence::None)
        }
    }

    /// Return every route ever observed for one logical thread. A healthy
    /// thread has exactly one. Keeping conflicts visible lets the data plane
    /// fail a child closed instead of choosing whichever parent execution was
    /// most recent.
    pub fn routes_for_thread(&self, thread: &CodexThreadKey) -> Vec<String> {
        let mut routes = self
            .thread_executions
            .get(thread)
            .into_iter()
            .flatten()
            .filter_map(|execution_id| self.executions.get(execution_id))
            .map(|binding| binding.route_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        routes.sort();
        routes
    }

    /// Bind an exact spawn call activity signal from Codex app-server.
    pub fn bind_spawn_activity(
        &mut self,
        activity: CodexSubagentActivity,
    ) -> Result<SpawnBindingResult, SubagentGraphError> {
        let parent = CodexThreadKey::new(activity.session_id.clone(), activity.parent_thread_id);
        let child = CodexThreadKey::new(activity.session_id, activity.child_thread_id);

        if parent == child {
            return Err(SubagentGraphError::SelfParent {
                thread: child.formatted(),
            });
        }

        // Cycle check before inserting spawn activity edge
        let mut check_ancestor = Some(&parent);
        let mut visited_ancestors = HashSet::new();
        visited_ancestors.insert(parent.clone());
        while let Some(curr) = check_ancestor {
            if curr == &child {
                return Err(SubagentGraphError::CycleDetected);
            }
            if let Some(node) = self.nodes.get(curr) {
                if let Some(next_p) = &node.parent {
                    if !visited_ancestors.insert(next_p.clone()) {
                        return Err(SubagentGraphError::CycleDetected);
                    }
                    check_ancestor = Some(next_p);
                } else {
                    check_ancestor = None;
                }
            } else {
                check_ancestor = None;
            }
        }

        if let Some(existing) = self.spawn_bindings.get(&activity.call_id) {
            if existing.parent == parent && existing.child == child {
                return Ok(SpawnBindingResult {
                    inserted: false,
                    duplicate: true,
                    conflict: false,
                });
            } else {
                return Err(SubagentGraphError::SpawnBindingConflict {
                    call_id: activity.call_id,
                });
            }
        }

        if let Some(existing_node) = self.nodes.get(&child) {
            if existing_node
                .parent
                .as_ref()
                .is_some_and(|existing_parent| existing_parent != &parent)
            {
                return Err(SubagentGraphError::ConflictingParent {
                    child: child.formatted(),
                });
            }
        }

        // Establish edge in graph
        self.children
            .entry(parent.clone())
            .or_default()
            .insert(child.clone());
        let node = self
            .nodes
            .entry(child.clone())
            .or_insert_with(|| SubagentThreadNode {
                key: child.clone(),
                parent: Some(parent.clone()),
                parent_turn_id: None,
                root_turn_id: None,
                forked_from_thread_id: None,
                agent_name: activity.agent_path,
                subagent_kind: None,
                latest_context_window_id: None,
                first_seen_ms: 0,
                last_seen_ms: 0,
                terminal: false,
            });
        if node.parent.is_none() {
            node.parent = Some(parent.clone());
        }

        self.spawn_bindings.insert(
            activity.call_id.clone(),
            SpawnCallBinding {
                parent,
                call_id: activity.call_id,
                child,
                observed_at_ms: 0,
                source: SpawnBindingSource::AppServerActivity,
            },
        );

        Ok(SpawnBindingResult {
            inserted: true,
            duplicate: false,
            conflict: false,
        })
    }

    /// Direct children of a parent thread.
    pub fn children_of(&self, parent: &CodexThreadKey) -> Vec<CodexThreadKey> {
        self.children
            .get(parent)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// All descendants of a root thread (cycle-safe, bounded BFS).
    pub fn descendants_of(
        &self,
        root: &CodexThreadKey,
        max_nodes: usize,
    ) -> Result<Vec<CodexThreadKey>, SubagentGraphError> {
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();

        visited.insert(root.clone());
        queue.push_back(root.clone());

        while let Some(current) = queue.pop_front() {
            if let Some(child_set) = self.children.get(&current) {
                for child in child_set {
                    if !visited.insert(child.clone()) {
                        return Err(SubagentGraphError::CycleDetected);
                    }
                    if result.len() >= max_nodes {
                        return Err(SubagentGraphError::TraversalLimitExceeded(max_nodes));
                    }
                    result.push(child.clone());
                    queue.push_back(child.clone());
                }
            }
        }

        Ok(result)
    }

    /// Get thread key bound to an execution ID.
    pub fn execution_thread(&self, execution_id: &str) -> Option<&CodexThreadKey> {
        self.executions.get(execution_id).map(|b| &b.thread)
    }

    /// Get full execution binding for an execution ID.
    pub fn execution_binding(&self, execution_id: &str) -> Option<&ThreadExecutionBinding> {
        self.executions.get(execution_id)
    }

    /// Active (non-terminal) execution IDs for a specific thread.
    pub fn active_executions_for_thread(&self, thread: &CodexThreadKey) -> Vec<String> {
        self.thread_executions
            .get(thread)
            .map(|execs| {
                execs
                    .iter()
                    .filter(|id| !self.execution_terminal.contains_key(*id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Check if a given request_id belongs to an execution on this thread.
    pub fn thread_has_request_id(&self, thread: &CodexThreadKey, request_id: &str) -> bool {
        self.thread_executions.get(thread).is_some_and(|execs| {
            execs.iter().any(|id| {
                self.executions
                    .get(id)
                    .is_some_and(|b| b.request_id == request_id)
            })
        })
    }

    /// Active descendant execution IDs for a root thread (for cancellation fanout).
    pub fn active_descendant_executions(
        &self,
        root: &CodexThreadKey,
        max_nodes: usize,
    ) -> Result<Vec<String>, SubagentGraphError> {
        let descendants = self.descendants_of(root, max_nodes)?;
        let mut active = Vec::new();
        for d in &descendants {
            active.extend(self.active_executions_for_thread(d));
        }
        Ok(active)
    }

    /// Mark an execution ID as terminal.
    pub fn mark_execution_terminal(&mut self, execution_id: &str, now_ms: u64) -> bool {
        self.execution_terminal
            .insert(execution_id.to_string(), now_ms)
            .is_none()
    }

    /// Remove a thread completely and clean all graph indexes.
    pub fn remove_thread(&mut self, key: &CodexThreadKey) -> bool {
        let Some(removed_node) = self.nodes.remove(key) else {
            return false;
        };

        // Remove from parent's children set
        if let Some(p) = &removed_node.parent {
            if let Some(children_set) = self.children.get_mut(p) {
                children_set.remove(key);
                if children_set.is_empty() {
                    self.children.remove(p);
                }
            }
        }

        // Detach children
        if let Some(children_set) = self.children.remove(key) {
            for child_key in children_set {
                if let Some(child_node) = self.nodes.get_mut(&child_key) {
                    if child_node.parent.as_ref() == Some(key) {
                        child_node.parent = None;
                    }
                }
            }
        }

        // Remove executions
        if let Some(exec_ids) = self.thread_executions.remove(key) {
            for exec_id in exec_ids {
                self.executions.remove(&exec_id);
                self.execution_terminal.remove(&exec_id);
            }
        }

        // Remove turn executions
        self.turn_executions.retain(|k, _| &k.thread != key);

        // Remove spawn bindings
        self.spawn_bindings
            .retain(|_, b| &b.parent != key && &b.child != key);

        true
    }

    /// Prune old terminal entries and bound registry memory.
    pub fn prune(&mut self, now_ms: u64, ttl_ms: u64, max_nodes: usize) -> PruneStats {
        let mut stats = PruneStats::default();

        // 1. Prune expired terminal executions
        let expired_execs: Vec<String> = self
            .execution_terminal
            .iter()
            .filter(|(_, terminal_time)| now_ms.saturating_sub(**terminal_time) > ttl_ms)
            .map(|(id, _)| id.clone())
            .collect();

        for exec_id in expired_execs {
            if let Some(binding) = self.executions.remove(&exec_id) {
                if let Some(set) = self.thread_executions.get_mut(&binding.thread) {
                    set.remove(&exec_id);
                }
                stats.executions_pruned += 1;
            }
            self.execution_terminal.remove(&exec_id);
        }

        // Clean empty turn executions
        self.turn_executions.retain(|_, set| {
            set.retain(|id| self.executions.contains_key(id));
            !set.is_empty()
        });

        // 2. Evict oldest inactive nodes if capacity exceeded (protect active execution threads)
        if self.nodes.len() > max_nodes {
            let mut inactive_node_entries: Vec<(CodexThreadKey, u64)> = self
                .nodes
                .iter()
                .filter(|(k, _)| self.active_executions_for_thread(k).is_empty())
                .map(|(k, v)| (k.clone(), v.last_seen_ms))
                .collect();
            inactive_node_entries.sort_by_key(|(_, ts)| *ts);

            for (k, _) in inactive_node_entries {
                if self.nodes.len() <= max_nodes {
                    break;
                }
                if self.remove_thread(&k) {
                    stats.nodes_pruned += 1;
                }
            }
        }

        stats
    }

    pub fn get_node(&self, thread: &CodexThreadKey) -> Option<&SubagentThreadNode> {
        self.nodes.get(thread)
    }

    /// Compute depth of a thread from its root (0 for root).
    pub fn node_depth(&self, thread: &CodexThreadKey, max_depth: usize) -> Option<u32> {
        let mut depth = 0u32;
        let mut current = thread.clone();
        let mut visited = HashSet::new();

        while depth as usize <= max_depth {
            visited.insert(current.clone());
            let node = self.nodes.get(&current)?;
            if let Some(parent) = &node.parent {
                if visited.contains(parent) {
                    return None; // Cycle detected
                }
                current = parent.clone();
                depth += 1;
            } else {
                return Some(depth);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Unit Tests (§22.3)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_turn_identity(session: &str, thread: &str, parent: Option<&str>) -> CodexTurnIdentity {
        CodexTurnIdentity {
            session_id: Some(CodexOpaqueId::new("session_id", session).unwrap()),
            thread_id: Some(CodexOpaqueId::new("thread_id", thread).unwrap()),
            parent_thread_id: parent.map(|p| CodexOpaqueId::new("parent_thread_id", p).unwrap()),
            source: CodexIdentitySource::TurnMetadataHeader,
            trust: if parent.is_some() {
                CodexIdentityTrust::Exact
            } else {
                CodexIdentityTrust::Structured
            },
            ..Default::default()
        }
    }

    fn dummy_obs(exec_id: &str, req_id: &str) -> TurnObservation {
        TurnObservation {
            execution_id: exec_id.into(),
            request_id: req_id.into(),
            connection_id: None,
            route_id: "test-route".into(),
            model: "test-model".into(),
            observed_at_ms: 1000,
        }
    }

    #[test]
    fn root_thread_has_no_parent_edge() {
        let mut registry = SubagentGraphRegistry::new();
        let identity = dummy_turn_identity("S1", "T_ROOT", None);
        let obs = dummy_obs("exec_1", "req_1");

        let res = registry.observe_turn(&identity, obs).unwrap();
        assert_eq!(res.parent, None);
        assert!(!res.edge_created);
        assert!(res.execution_bound);

        let root_key = res.thread.unwrap();
        assert_eq!(registry.children_of(&root_key).len(), 0);
        assert_eq!(registry.node_depth(&root_key, 10), Some(0));
    }

    #[test]
    fn child_metadata_creates_exact_parent_edge() {
        let mut registry = SubagentGraphRegistry::new();
        let child_id = dummy_turn_identity("S1", "T_CHILD", Some("T_PARENT"));
        let obs = dummy_obs("exec_c", "req_c");

        let res = registry.observe_turn(&child_id, obs).unwrap();
        assert!(res.edge_created);
        let parent_key = res.parent.unwrap();
        let child_key = res.thread.unwrap();

        assert_eq!(parent_key.thread_id.as_str(), "T_PARENT");
        assert_eq!(child_key.thread_id.as_str(), "T_CHILD");
        assert_eq!(registry.children_of(&parent_key), vec![child_key.clone()]);
        assert_eq!(registry.node_depth(&child_key, 10), Some(1));
    }

    #[test]
    fn nested_child_creates_parent_child_grandchild_graph() {
        let mut registry = SubagentGraphRegistry::new();
        let root = dummy_turn_identity("S1", "P", None);
        registry
            .observe_turn(&root, dummy_obs("e_p", "r_p"))
            .unwrap();

        let child = dummy_turn_identity("S1", "C", Some("P"));
        registry
            .observe_turn(&child, dummy_obs("e_c", "r_c"))
            .unwrap();

        let grandchild = dummy_turn_identity("S1", "G", Some("C"));
        registry
            .observe_turn(&grandchild, dummy_obs("e_g", "r_g"))
            .unwrap();

        let p_key = CodexThreadKey::new(
            CodexOpaqueId::new("s", "S1").unwrap(),
            CodexOpaqueId::new("t", "P").unwrap(),
        );
        let descendants = registry.descendants_of(&p_key, 10).unwrap();
        assert_eq!(descendants.len(), 2);
        assert_eq!(descendants[0].thread_id.as_str(), "C");
        assert_eq!(descendants[1].thread_id.as_str(), "G");
    }

    #[test]
    fn repeated_same_turn_observation_is_idempotent() {
        let mut registry = SubagentGraphRegistry::new();
        let child = dummy_turn_identity("S1", "C", Some("P"));

        let res1 = registry
            .observe_turn(&child, dummy_obs("e1", "r1"))
            .unwrap();
        assert!(res1.edge_created);

        let res2 = registry
            .observe_turn(&child, dummy_obs("e2", "r2"))
            .unwrap();
        assert!(!res2.edge_created); // Already exists
    }

    #[test]
    fn parallel_children_with_identical_prompts_remain_distinct_by_thread_id() {
        let mut registry = SubagentGraphRegistry::new();
        let child1 = dummy_turn_identity("S1", "C1", Some("P"));
        let child2 = dummy_turn_identity("S1", "C2", Some("P"));

        registry
            .observe_turn(&child1, dummy_obs("e_c1", "r_c1"))
            .unwrap();
        registry
            .observe_turn(&child2, dummy_obs("e_c2", "r_c2"))
            .unwrap();

        let p_key = CodexThreadKey::new(
            CodexOpaqueId::new("s", "S1").unwrap(),
            CodexOpaqueId::new("t", "P").unwrap(),
        );
        let children = registry.children_of(&p_key);
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn existing_exact_parent_is_never_rewritten_by_conflicting_metadata() {
        let mut registry = SubagentGraphRegistry::new();
        let child1 = dummy_turn_identity("S1", "C", Some("P1"));
        registry
            .observe_turn(&child1, dummy_obs("e1", "r1"))
            .unwrap();

        let child_conflict = dummy_turn_identity("S1", "C", Some("P2"));
        let err = registry
            .observe_turn(&child_conflict, dummy_obs("e2", "r2"))
            .unwrap_err();
        assert_eq!(
            err,
            SubagentGraphError::ConflictingParent {
                child: "S1:C".into()
            }
        );
    }

    #[test]
    fn self_parent_edge_is_rejected() {
        let mut registry = SubagentGraphRegistry::new();
        let self_parent = dummy_turn_identity("S1", "SELF", Some("SELF"));
        let err = registry
            .observe_turn(&self_parent, dummy_obs("e", "r"))
            .unwrap_err();
        assert_eq!(
            err,
            SubagentGraphError::SelfParent {
                thread: "S1:SELF".into()
            }
        );
    }

    #[test]
    fn descendants_traversal_detects_cycle_and_stays_bounded() {
        let mut registry = SubagentGraphRegistry::new();
        let k1 = CodexThreadKey::new(
            CodexOpaqueId::new("s", "S").unwrap(),
            CodexOpaqueId::new("t", "1").unwrap(),
        );
        let k2 = CodexThreadKey::new(
            CodexOpaqueId::new("s", "S").unwrap(),
            CodexOpaqueId::new("t", "2").unwrap(),
        );

        // Forge a cycle in children
        registry
            .children
            .entry(k1.clone())
            .or_default()
            .insert(k2.clone());
        registry
            .children
            .entry(k2.clone())
            .or_default()
            .insert(k1.clone());

        let err = registry.descendants_of(&k1, 10).unwrap_err();
        assert_eq!(err, SubagentGraphError::CycleDetected);
    }

    #[test]
    fn execution_can_be_resolved_back_to_thread() {
        let mut registry = SubagentGraphRegistry::new();
        let child = dummy_turn_identity("S1", "C", Some("P"));
        registry
            .observe_turn(&child, dummy_obs("exec_test", "req_test"))
            .unwrap();

        let found = registry.execution_thread("exec_test").unwrap();
        assert_eq!(found.thread_id.as_str(), "C");
    }

    #[test]
    fn duplicate_app_server_spawn_binding_is_idempotent() {
        let mut registry = SubagentGraphRegistry::new();
        let act = CodexSubagentActivity {
            session_id: CodexOpaqueId::new("s", "S").unwrap(),
            parent_thread_id: CodexOpaqueId::new("p", "P").unwrap(),
            call_id: "call_123".into(),
            child_thread_id: CodexOpaqueId::new("c", "C").unwrap(),
            agent_path: None,
            status: CodexSubagentActivityStatus::Started,
        };

        let r1 = registry.bind_spawn_activity(act.clone()).unwrap();
        assert!(r1.inserted);

        let r2 = registry.bind_spawn_activity(act).unwrap();
        assert!(r2.duplicate);
        assert!(!r2.inserted);
    }

    #[test]
    fn one_call_id_cannot_bind_to_two_children() {
        let mut registry = SubagentGraphRegistry::new();
        let act1 = CodexSubagentActivity {
            session_id: CodexOpaqueId::new("s", "S").unwrap(),
            parent_thread_id: CodexOpaqueId::new("p", "P").unwrap(),
            call_id: "call_xyz".into(),
            child_thread_id: CodexOpaqueId::new("c", "C1").unwrap(),
            agent_path: None,
            status: CodexSubagentActivityStatus::Started,
        };
        registry.bind_spawn_activity(act1).unwrap();

        let act2 = CodexSubagentActivity {
            session_id: CodexOpaqueId::new("s", "S").unwrap(),
            parent_thread_id: CodexOpaqueId::new("p", "P").unwrap(),
            call_id: "call_xyz".into(),
            child_thread_id: CodexOpaqueId::new("c", "C2").unwrap(),
            agent_path: None,
            status: CodexSubagentActivityStatus::Started,
        };
        let err = registry.bind_spawn_activity(act2).unwrap_err();
        assert_eq!(
            err,
            SubagentGraphError::SpawnBindingConflict {
                call_id: "call_xyz".into()
            }
        );
    }

    #[test]
    fn app_server_activity_cannot_give_an_existing_child_a_second_parent() {
        let mut registry = SubagentGraphRegistry::new();
        let child = dummy_turn_identity("session", "child", Some("parent_a"));
        registry
            .observe_turn(&child, dummy_obs("exec_child", "req_child"))
            .unwrap();

        let activity = CodexSubagentActivity {
            session_id: CodexOpaqueId::new("session_id", "session").unwrap(),
            parent_thread_id: CodexOpaqueId::new("parent_thread_id", "parent_b").unwrap(),
            call_id: "call_conflict".into(),
            child_thread_id: CodexOpaqueId::new("child_thread_id", "child").unwrap(),
            agent_path: None,
            status: CodexSubagentActivityStatus::Started,
        };

        assert_eq!(
            registry.bind_spawn_activity(activity),
            Err(SubagentGraphError::ConflictingParent {
                child: "session:child".into(),
            })
        );
        let parent_b = CodexThreadKey::new(
            CodexOpaqueId::new("session_id", "session").unwrap(),
            CodexOpaqueId::new("thread_id", "parent_b").unwrap(),
        );
        assert!(registry.children_of(&parent_b).is_empty());
    }

    #[test]
    fn terminal_old_nodes_and_bindings_are_pruned_with_capacity_bound() {
        let mut registry = SubagentGraphRegistry::new();
        let child = dummy_turn_identity("S1", "C", Some("P"));
        registry
            .observe_turn(&child, dummy_obs("exec_old", "req_old"))
            .unwrap();

        registry.mark_execution_terminal("exec_old", 1000);
        let stats = registry.prune(1000 + DEFAULT_GRAPH_TTL_MS + 500, DEFAULT_GRAPH_TTL_MS, 100);
        assert_eq!(stats.executions_pruned, 1);
        assert!(registry.execution_thread("exec_old").is_none());
    }

    #[test]
    fn insertion_time_cycle_prevention_rejects_cycle_before_graph_mutation() {
        let mut registry = SubagentGraphRegistry::new();
        // C is child of P
        let child = dummy_turn_identity("S1", "C", Some("P"));
        registry
            .observe_turn(&child, dummy_obs("exec_c", "req_c"))
            .unwrap();

        // Now attempt to make P a child of C -> should detect cycle at insertion time
        let cycle_parent = dummy_turn_identity("S1", "P", Some("C"));
        let err = registry
            .observe_turn(&cycle_parent, dummy_obs("exec_p", "req_p"))
            .unwrap_err();
        assert_eq!(err, SubagentGraphError::CycleDetected);

        // Verify graph was not corrupted
        let p_key = CodexThreadKey::new(
            CodexOpaqueId::new("s", "S1").unwrap(),
            CodexOpaqueId::new("t", "P").unwrap(),
        );
        let c_key = CodexThreadKey::new(
            CodexOpaqueId::new("s", "S1").unwrap(),
            CodexOpaqueId::new("t", "C").unwrap(),
        );
        assert_eq!(registry.get_node(&p_key).unwrap().parent, None);
        assert_eq!(registry.children_of(&c_key).len(), 0);
    }

    #[test]
    fn exact_parent_turn_execution_resolution() {
        let mut registry = SubagentGraphRegistry::new();
        let s_id = CodexOpaqueId::new("s", "S1").unwrap();
        let p_id = CodexOpaqueId::new("t", "P").unwrap();
        let t1_id = CodexOpaqueId::new("turn", "turn_1").unwrap();
        let t2_id = CodexOpaqueId::new("turn", "turn_2").unwrap();

        // Parent turn 1 execution
        let mut parent_turn1 = dummy_turn_identity("S1", "P", None);
        parent_turn1.turn_id = Some(t1_id.clone());
        registry
            .observe_turn(&parent_turn1, dummy_obs("exec_parent_t1", "req_1"))
            .unwrap();

        // Parent turn 2 execution
        let mut parent_turn2 = dummy_turn_identity("S1", "P", None);
        parent_turn2.turn_id = Some(t2_id.clone());
        registry
            .observe_turn(&parent_turn2, dummy_obs("exec_parent_t2", "req_2"))
            .unwrap();

        // Child spawned from turn 2
        let (resolved, conf) = registry.resolve_parent_executions(&s_id, &p_id, Some(&t2_id));
        assert_eq!(conf, LinkConfidence::High);
        assert_eq!(resolved, vec!["exec_parent_t2".to_string()]);

        // Child spawned from turn 1
        let (resolved, conf) = registry.resolve_parent_executions(&s_id, &p_id, Some(&t1_id));
        assert_eq!(conf, LinkConfidence::High);
        assert_eq!(resolved, vec!["exec_parent_t1".to_string()]);

        // Child spawned with unknown turn_id fails closed (never falls back to thread)
        let t_unknown = CodexOpaqueId::new("turn", "turn_unknown").unwrap();
        let (resolved, conf) = registry.resolve_parent_executions(&s_id, &p_id, Some(&t_unknown));
        assert_eq!(conf, LinkConfidence::None);
        assert!(resolved.is_empty());

        // Child with None turn_id and >1 active parent executions is ambiguous -> fails closed
        let (resolved_none, conf_none) = registry.resolve_parent_executions(&s_id, &p_id, None);
        assert_eq!(conf_none, LinkConfidence::None);
        assert!(resolved_none.is_empty());
    }

    #[test]
    fn remove_thread_cleans_all_indexes() {
        let mut registry = SubagentGraphRegistry::new();
        let s_id = CodexOpaqueId::new("s", "S1").unwrap();
        let p_id = CodexOpaqueId::new("t", "P").unwrap();
        let c_id = CodexOpaqueId::new("t", "C").unwrap();

        let parent = dummy_turn_identity("S1", "P", None);
        registry
            .observe_turn(&parent, dummy_obs("e_p", "r_p"))
            .unwrap();

        let child = dummy_turn_identity("S1", "C", Some("P"));
        registry
            .observe_turn(&child, dummy_obs("e_c", "r_c"))
            .unwrap();

        let c_key = CodexThreadKey::new(s_id.clone(), c_id.clone());
        let p_key = CodexThreadKey::new(s_id, p_id);

        assert!(registry.remove_thread(&c_key));
        assert!(registry.get_node(&c_key).is_none());
        assert!(!registry.children_of(&p_key).contains(&c_key));
        assert!(registry.execution_thread("e_c").is_none());
        assert!(registry.execution_binding("e_c").is_none());
    }
}
