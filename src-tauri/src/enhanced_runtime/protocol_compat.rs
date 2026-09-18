//! Whether the Codex Desktop on this machine can be served by the Enhanced
//! core this release pinned.
//!
//! The gate this replaces compared one SHA-256 over the whole generated schema
//! set against a hash compiled into the binary. That hash answers "is this the
//! exact Codex we pinned", which is the right question for a qualification
//! marker and the wrong one for an arming decision: Codex Desktop updates
//! itself, so the hash goes stale on its own, and any change anywhere in ~300
//! schema files — a new unrelated method, one added optional field — disarmed
//! Enhanced completely.
//!
//! What actually has to hold is narrower. The bridge is a relay: for a thread
//! bound to the Enhanced plane, everything Desktop sends reaches the Enhanced
//! child, so the question is whether the child's protocol can still serve the
//! client's. Both sides can be asked directly — `app-server
//! generate-json-schema` is a subcommand of every Codex core, the Enhanced
//! fork included, and the fork's output is byte-identical to the upstream it
//! was built from, because its seams are in the agent loop and not on the
//! wire. So nothing has to be snapshotted into the repository: this probes the
//! two binaries that are actually going to run.
//!
//! Severity is graded rather than binary. A difference is fatal only when it
//! lands on [`ROUTED_METHODS`] — the handful the bridge itself must
//! demultiplex, without which no Enhanced thread can start or finish. Every
//! other difference is real and reported by name, but it degrades one
//! capability rather than breaking routing, so it produces
//! [`ProtocolVerdict::Unverified`]: Enhanced runs, and the UI has to say the
//! combination was never qualified.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::process::background_command;

/// The methods the bridge itself reads and routes on. Losing one of these, or
/// having its wire shape break, means no Enhanced thread can be started,
/// resumed, or completed — there is no degraded mode to offer.
///
/// Kept as method names rather than schema type names because that is what the
/// bridge matches on, and checked only when the pinned core actually declared
/// the method: `turn/create` appears in the bridge as a legacy branch and is in
/// no current schema, so a fatal rule keyed on the live side alone would fire
/// forever on a method nothing has sent for releases.
pub const ROUTED_METHODS: &[&str] = super::contracts::REQUIRED_METHODS;

/// The four top-level envelopes Codex generates, and who writes each one.
///
/// Direction decides what counts as breaking, and it is not symmetric. On a
/// client-to-child message the child is the reader: a field the client stopped
/// sending can break it, a field the client added cannot. On a child-to-client
/// message the roles swap: a field the client newly *requires* breaks, because
/// the older child will not emit it.
const ENVELOPES: &[(&str, Direction)] = &[
    ("ClientRequest", Direction::ToChild),
    ("ClientNotification", Direction::ToChild),
    ("ServerRequest", Direction::ToClient),
    ("ServerNotification", Direction::ToClient),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    /// Desktop writes it, the Enhanced child reads it.
    ToChild,
    /// The Enhanced child writes it, Desktop reads it.
    ToClient,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Shape {
    pub required: BTreeSet<String>,
    pub properties: BTreeSet<String>,
}

/// One Codex core's protocol, as it describes itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolSurface {
    pub documents: super::schema_contract::Documents,
    /// Measured, not pinned. An exact match short-circuits the whole
    /// comparison; a mismatch is the beginning of the question, not the answer.
    pub schema_sha256: String,
    pub methods: BTreeMap<String, BTreeSet<String>>,
    /// Type name to shape, flattened across every generated file. Codex names
    /// each root by its `title` and every shared sub-type under `definitions`,
    /// and those names are stable across versions in a way file paths and JSON
    /// pointers are not.
    pub shapes: BTreeMap<String, Shape>,
}

impl ProtocolSurface {
    /// Runs `app-server generate-json-schema` and reads back what it wrote.
    pub fn probe(binary: &Path) -> Result<Self, ProtocolCompatError> {
        let output = tempfile::tempdir().map_err(ProtocolCompatError::Io)?;
        let mut child = background_command(binary)
            .args(["app-server", "generate-json-schema", "--out"])
            .arg(output.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                ProtocolCompatError::Probe(format!("{}: {error}", binary.display()))
            })?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                outcome => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProtocolCompatError::Probe(match outcome {
                        Err(error) => format!("schema probe failed: {error}"),
                        _ => "schema probe timed out after 30 seconds".into(),
                    }));
                }
            }
        };
        if !status.success() {
            return Err(ProtocolCompatError::Probe(format!(
                "{} returned {status}",
                binary.display()
            )));
        }
        Self::read_dir(output.path())
    }

    fn read_dir(root: &Path) -> Result<Self, ProtocolCompatError> {
        let mut files = Vec::new();
        collect_files(root, root, &mut files)?;
        files.sort_by(|left, right| left.0.cmp(&right.0));
        if files.is_empty() {
            return Err(ProtocolCompatError::Probe("schema output was empty".into()));
        }

        // Byte-for-byte over sorted relative paths. `desktop_codex` computes
        // the same digest for its own identity probe; `hash_matches_desktop_probe`
        // holds the two together.
        let mut hasher = Sha256::new();
        let mut shapes: BTreeMap<String, Shape> = BTreeMap::new();
        let mut methods: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut documents = BTreeMap::new();
        for (relative, path) in &files {
            let bytes = std::fs::read(path).map_err(ProtocolCompatError::Io)?;
            hasher.update(relative.as_bytes());
            hasher.update([0]);
            hasher.update(&bytes);

            let document = serde_json::from_slice::<Value>(&bytes).map_err(|_| {
                ProtocolCompatError::Probe(format!("invalid schema document: {relative}"))
            })?;
            documents.insert(relative.clone(), document.clone());
            collect_shapes(&document, relative.trim_end_matches(".json"), &mut shapes);
            if let Some(envelope) = relative.strip_suffix(".json") {
                if ENVELOPES.iter().any(|(name, _)| *name == envelope) {
                    methods.insert(envelope.to_string(), collect_methods(&document));
                }
            }
        }

        for (envelope, _) in ENVELOPES {
            if methods.get(*envelope).is_none_or(BTreeSet::is_empty) {
                return Err(ProtocolCompatError::Probe(format!(
                    "missing or unparseable envelope: {envelope}"
                )));
            }
        }
        Ok(Self {
            documents,
            schema_sha256: hex::encode(hasher.finalize()),
            methods,
            shapes,
        })
    }

    fn methods_in(&self, envelope: &str) -> BTreeSet<String> {
        self.methods.get(envelope).cloned().unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProtocolVerdict {
    /// Identical protocol. This is the pinned combination.
    Verified,
    /// Differences exist, none of them on a routed method. Enhanced can run;
    /// nobody has qualified this pairing.
    Unverified,
    /// A method the bridge routes is gone or its wire shape broke.
    Incompatible,
}

impl ProtocolVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Unverified => "unverified",
            Self::Incompatible => "incompatible",
        }
    }

    /// Whether Enhanced may be armed. Only [`Self::Incompatible`] withholds it
    /// — an unverified pairing is arming plus a statement, not a refusal.
    pub fn may_arm(self) -> bool {
        !matches!(self, Self::Incompatible)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeltaKind {
    ShapeIncompatible,
    ShapeUnverified,
    /// Desktop can send a method the pinned core never implemented.
    MethodUnservable,
    /// The pinned core can send a method Desktop no longer declares.
    MethodUnknownToDesktop,
    /// Desktop now requires a field the pinned core does not emit.
    FieldNewlyRequired,
    /// Desktop no longer sends a field the pinned core reads.
    FieldRemoved,
}

impl DeltaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ShapeIncompatible => "shapeIncompatible",
            Self::ShapeUnverified => "shapeUnverified",
            Self::MethodUnservable => "methodUnservable",
            Self::MethodUnknownToDesktop => "methodUnknownToDesktop",
            Self::FieldNewlyRequired => "fieldNewlyRequired",
            Self::FieldRemoved => "fieldRemoved",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolDelta {
    pub kind: DeltaKind,
    /// The method or type name the difference is on.
    pub subject: String,
    /// Field names, when the difference is field-level.
    pub fields: Vec<String>,
    /// On a method the bridge itself must route. Proven incompatibilities on
    /// routed methods are fatal; an indeterminate shape remains unverified
    /// evidence and must not be promoted into a break merely because the
    /// bridge observes that method.
    pub routed: bool,
}

impl ProtocolDelta {
    fn blocks_adoption(&self) -> bool {
        self.routed && self.kind != DeltaKind::ShapeUnverified
    }

    /// A one-line rendering for logs and blocker text. The UI has its own
    /// localized wording and uses the structured fields instead.
    pub fn describe(&self) -> String {
        let subject = &self.subject;
        let fields = self.fields.join(", ");
        match self.kind {
            DeltaKind::ShapeIncompatible => {
                format!("{subject} wire contract is incompatible: {fields}")
            }
            DeltaKind::ShapeUnverified => {
                format!("{subject} wire contract could not be verified: {fields}")
            }
            DeltaKind::MethodUnservable => {
                format!("Desktop may call {subject}, which the Enhanced core does not implement")
            }
            DeltaKind::MethodUnknownToDesktop => {
                format!("the Enhanced core may send {subject}, which Desktop no longer declares")
            }
            DeltaKind::FieldNewlyRequired => {
                format!("Desktop now requires {subject}.{fields}, which the Enhanced core does not emit")
            }
            DeltaKind::FieldRemoved => {
                format!("Desktop no longer sends {subject}.{fields}, which the Enhanced core reads")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolCompatibility {
    pub verdict: ProtocolVerdict,
    /// Digest of the core this release pinned.
    pub pinned_schema_sha256: String,
    /// Digest of the Codex Desktop core found on this machine.
    pub desktop_schema_sha256: String,
    pub deltas: Vec<ProtocolDelta>,
}

impl ProtocolCompatibility {
    /// Every difference on a method observed by the bridge, including
    /// indeterminate shape comparisons that are diagnostic-only.
    pub fn routed_deltas(&self) -> impl Iterator<Item = &ProtocolDelta> {
        self.deltas.iter().filter(|delta| delta.routed)
    }

    /// Proven routed breaks that actually withhold Enhanced adoption.
    pub fn blocking_deltas(&self) -> impl Iterator<Item = &ProtocolDelta> {
        self.deltas.iter().filter(|delta| delta.blocks_adoption())
    }
}

/// Compares the core this release pinned against the Codex Desktop installed
/// here.
///
/// `pinned` is the Enhanced core rather than a stored snapshot: it is the
/// binary that will actually serve Enhanced threads, and asking it removes a
/// whole class of "the snapshot says one thing, the file says another".
pub fn compare(pinned: &ProtocolSurface, desktop: &ProtocolSurface) -> ProtocolCompatibility {
    if pinned.schema_sha256 == desktop.schema_sha256
        && pinned.documents.is_empty()
        && desktop.documents.is_empty()
    {
        return ProtocolCompatibility {
            verdict: ProtocolVerdict::Verified,
            pinned_schema_sha256: pinned.schema_sha256.clone(),
            desktop_schema_sha256: desktop.schema_sha256.clone(),
            deltas: Vec::new(),
        };
    }

    let mut deltas = Vec::new();
    for (envelope, direction) in ENVELOPES {
        let ours = pinned.methods_in(envelope);
        let theirs = desktop.methods_in(envelope);
        match direction {
            // Desktop gained a call the pinned core cannot answer.
            Direction::ToChild => {
                for method in theirs.difference(&ours) {
                    deltas.push(ProtocolDelta {
                        kind: DeltaKind::MethodUnservable,
                        subject: method.clone(),
                        fields: Vec::new(),
                        // Only a method we already routed can break routing; a
                        // brand new one cannot, because nothing bound to it.
                        routed: false,
                    });
                }
                // A routed method the pinned core declares and Desktop dropped
                // is the fatal case: the bridge demultiplexes on it.
                for method in ours.difference(&theirs) {
                    let routed = ROUTED_METHODS.contains(&method.as_str());
                    if !routed {
                        continue;
                    }
                    deltas.push(ProtocolDelta {
                        kind: DeltaKind::MethodUnknownToDesktop,
                        subject: method.clone(),
                        fields: Vec::new(),
                        routed,
                    });
                }
            }
            Direction::ToClient => {
                for method in ours.difference(&theirs) {
                    deltas.push(ProtocolDelta {
                        kind: DeltaKind::MethodUnknownToDesktop,
                        subject: method.clone(),
                        fields: Vec::new(),
                        routed: ROUTED_METHODS.contains(&method.as_str()),
                    });
                }
            }
        }
    }

    if !pinned.documents.is_empty() && !desktop.documents.is_empty() {
        use super::schema_contract::{compare_method, compare_named, Inclusion};
        let mut record = |subject: &str, result: Inclusion| {
            let (kind, reason) = match result {
                Inclusion::Compatible => return,
                Inclusion::Breaking(reason) => (DeltaKind::ShapeIncompatible, reason),
                Inclusion::Unknown(reason) => (DeltaKind::ShapeUnverified, reason),
            };
            deltas.push(ProtocolDelta {
                kind,
                subject: subject.into(),
                fields: vec![reason],
                routed: ROUTED_METHODS.contains(&subject),
            });
        };
        for (envelope, direction) in ENVELOPES {
            let (sender, reader) = match direction {
                Direction::ToChild => (desktop, pinned),
                Direction::ToClient => (pinned, desktop),
            };
            for method in sender
                .methods_in(envelope)
                .intersection(&reader.methods_in(envelope))
            {
                record(
                    method,
                    compare_method(&sender.documents, &reader.documents, envelope, method),
                );
            }
        }
        for (method, name, core_writes) in super::contracts::RESPONSES {
            let (sender, reader) = if *core_writes {
                (pinned, desktop)
            } else {
                (desktop, pinned)
            };
            record(
                method,
                compare_named(&sender.documents, &reader.documents, name),
            );
        }
    } else {
        for (name, ours) in &pinned.shapes {
            let Some(theirs) = desktop.shapes.get(name) else {
                continue;
            };
            let routed = shape_is_routed(name);
            let direction = shape_direction(name);

            if matches!(direction, Direction::ToClient) {
                let newly_required: Vec<String> = theirs
                    .required
                    .difference(&ours.required)
                    .cloned()
                    .collect();
                if !newly_required.is_empty() {
                    deltas.push(ProtocolDelta {
                        kind: DeltaKind::FieldNewlyRequired,
                        subject: name.clone(),
                        fields: newly_required,
                        routed,
                    });
                }
            }

            // A property the pinned core reads and Desktop stopped sending breaks
            // in either direction: as a request the child can no longer parse, or
            // as a response the child's own reader will miss.
            let removed: Vec<String> = ours
                .properties
                .difference(&theirs.properties)
                .cloned()
                .collect();
            if !removed.is_empty() {
                deltas.push(ProtocolDelta {
                    kind: DeltaKind::FieldRemoved,
                    subject: name.clone(),
                    fields: removed,
                    routed,
                });
            }
        }
    }
    deltas.sort_by(|left, right| {
        right
            .routed
            .cmp(&left.routed)
            .then_with(|| left.subject.cmp(&right.subject))
    });

    let verdict = if deltas.iter().any(ProtocolDelta::blocks_adoption) {
        ProtocolVerdict::Incompatible
    } else if deltas.is_empty() && pinned.schema_sha256 == desktop.schema_sha256 {
        ProtocolVerdict::Verified
    } else {
        ProtocolVerdict::Unverified
    };
    ProtocolCompatibility {
        verdict,
        pinned_schema_sha256: pinned.schema_sha256.clone(),
        desktop_schema_sha256: desktop.schema_sha256.clone(),
        deltas,
    }
}

/// Type names Codex derives from a routed method, by its own convention:
/// `thread/start` produces `ThreadStartParams` and `ThreadStartResponse`,
/// `turn/completed` produces `TurnCompletedNotification`.
fn shape_is_routed(type_name: &str) -> bool {
    let root = shape_root(type_name);
    ROUTED_METHODS.iter().any(|method| {
        let stem = method_type_stem(method);
        !stem.is_empty()
            && root.starts_with(&stem)
            && matches!(&root[stem.len()..], "Params" | "Response" | "Notification")
    })
}

/// Who writes a shape, which decides whether a newly required field is a break.
///
/// The envelope types say so outright and are checked first: everything under
/// `ClientRequest` is written by Desktop however its own variants are named,
/// and reading direction off the `Params` suffix there gets it exactly
/// backwards — Desktop tightening `params` from optional to required on six of
/// its own request variants was reported as six things the Enhanced core fails
/// to emit, when the core never emits them at all.
fn shape_direction(type_name: &str) -> Direction {
    let root = shape_root(type_name);
    if let Some((_, direction)) = ENVELOPES.iter().find(|(name, _)| *name == root) {
        return *direction;
    }
    if root.ends_with("Params") {
        Direction::ToChild
    } else {
        Direction::ToClient
    }
}

/// The named type an anonymous shape sits inside. `ThreadStartResponse/oneOf[2]`
/// is routed and read by the client for the same reasons its root is.
fn shape_root(type_name: &str) -> &str {
    let end = type_name.find(['/', '[']).unwrap_or(type_name.len());
    &type_name[..end]
}

fn method_type_stem(method: &str) -> String {
    method
        .split('/')
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn collect_methods(document: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    fn walk(node: &Value, out: &mut BTreeSet<String>) {
        match node {
            Value::Object(map) => {
                if let Some(method) = map.get("properties").and_then(|p| p.get("method")) {
                    if let Some(constant) = method.get("const").and_then(Value::as_str) {
                        out.insert(constant.to_string());
                    }
                    if let Some(values) = method.get("enum").and_then(Value::as_array) {
                        out.extend(values.iter().filter_map(Value::as_str).map(String::from));
                    }
                }
                for value in map.values() {
                    walk(value, out);
                }
            }
            Value::Array(values) => {
                for value in values {
                    walk(value, out);
                }
            }
            _ => {}
        }
    }
    walk(document, &mut out);
    out
}

/// Flattens every object shape in one generated file into the surface map.
///
/// Three kinds of shape have to be caught, and the third is the one that made
/// this more than a two-line function. Named types — the file's root `title`
/// and each entry under `definitions` — are keyed by that name, so they compare
/// across versions and across the files that repeat them. But Codex also emits
/// *anonymous* shapes: the members of a `oneOf` union are inline objects with
/// no title and no definition entry.
///
/// Those are not a rare corner. `McpServerElicitationRequestParams` keeps an
/// identical root and identical definitions between 0.151 and 0.153 while its
/// third union member swaps `elicitationId` + `url` for `requestedSchema` — a
/// real wire change that a name-keyed walk reports as nothing at all. So they
/// are keyed by enclosing type plus JSON pointer. Variant order is what makes
/// that comparable, and when order does shift the mismatch surfaces as a delta
/// rather than as silence, which is the failure direction to prefer.
fn collect_shapes(document: &Value, fallback_name: &str, out: &mut BTreeMap<String, Shape>) {
    let base = document
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(fallback_name)
        .to_string();

    if let Some(definitions) = document
        .get("definitions")
        .or_else(|| document.get("$defs"))
        .and_then(Value::as_object)
    {
        for (name, definition) in definitions {
            collect_anonymous(definition, name, out);
        }
    }
    collect_anonymous(document, &base, out);
}

fn collect_anonymous(node: &Value, key: &str, out: &mut BTreeMap<String, Shape>) {
    if let Some(shape) = shape_of(node) {
        out.insert(key.to_string(), shape);
    }
    match node {
        Value::Object(map) => {
            for (name, value) in map {
                // Handled by name at the top level; descending would key the
                // same shared types twice, once under a misleading pointer.
                if name == "definitions" || name == "$defs" {
                    continue;
                }
                collect_anonymous(value, &format!("{key}/{name}"), out);
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                collect_anonymous(value, &format!("{key}[{index}]"), out);
            }
        }
        _ => {}
    }
}

fn shape_of(node: &Value) -> Option<Shape> {
    let properties = node.get("properties")?.as_object()?;
    Some(Shape {
        required: node
            .get("required")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        properties: properties.keys().cloned().collect(),
    })
}

fn collect_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), ProtocolCompatError> {
    for entry in std::fs::read_dir(current).map_err(ProtocolCompatError::Io)? {
        let entry = entry.map_err(ProtocolCompatError::Io)?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            files.push((relative, path));
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolCompatError {
    #[error("protocol probe failed: {0}")]
    Probe(String),
    #[error("protocol probe io: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(
        methods: &[(&str, &[&str])],
        shapes: &[(&str, &[&str], &[&str])],
        digest: &str,
    ) -> ProtocolSurface {
        ProtocolSurface {
            documents: BTreeMap::new(),
            schema_sha256: digest.into(),
            methods: methods
                .iter()
                .map(|(envelope, names)| {
                    (
                        (*envelope).to_string(),
                        names.iter().map(|name| (*name).to_string()).collect(),
                    )
                })
                .collect(),
            shapes: shapes
                .iter()
                .map(|(name, required, properties)| {
                    (
                        (*name).to_string(),
                        Shape {
                            required: required.iter().map(|f| (*f).to_string()).collect(),
                            properties: properties.iter().map(|f| (*f).to_string()).collect(),
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn an_identical_protocol_is_verified_without_comparing_anything() {
        let pinned = surface(&[("ClientRequest", &["thread/start"])], &[], "same");
        let desktop = surface(&[("ClientRequest", &["turn/start"])], &[], "same");
        let result = compare(&pinned, &desktop);
        assert_eq!(result.verdict, ProtocolVerdict::Verified);
        assert!(result.deltas.is_empty());
    }

    /// The case this whole module exists for: Codex Desktop grew a method the
    /// pinned core never had. That costs one capability, not routing.
    #[test]
    fn a_method_desktop_added_is_unverified_rather_than_incompatible() {
        let pinned = surface(&[("ClientRequest", &["thread/start"])], &[], "old");
        let desktop = surface(
            &[("ClientRequest", &["thread/start", "plugin/reconcile"])],
            &[],
            "new",
        );
        let result = compare(&pinned, &desktop);
        assert_eq!(result.verdict, ProtocolVerdict::Unverified);
        assert!(result.verdict.may_arm());
        assert_eq!(result.deltas.len(), 1);
        assert_eq!(result.deltas[0].kind, DeltaKind::MethodUnservable);
        assert_eq!(result.deltas[0].subject, "plugin/reconcile");
        assert!(!result.deltas[0].routed);
    }

    #[test]
    fn losing_a_routed_method_is_incompatible() {
        let pinned = surface(
            &[("ClientRequest", &["thread/start", "thread/resume"])],
            &[],
            "old",
        );
        let desktop = surface(&[("ClientRequest", &["thread/start"])], &[], "new");
        let result = compare(&pinned, &desktop);
        assert_eq!(result.verdict, ProtocolVerdict::Incompatible);
        assert!(!result.verdict.may_arm());
        assert_eq!(result.routed_deltas().count(), 1);
        assert_eq!(result.blocking_deltas().count(), 1);
    }

    #[test]
    fn an_indeterminate_routed_union_is_unverified_and_may_arm() {
        let request = |params: Value, digest: &str| ProtocolSurface {
            documents: [(
                "ClientRequest.json".into(),
                serde_json::json!({
                    "title": "ClientRequest",
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "method": {"const": "thread/start"},
                        "params": params
                    },
                    "required": ["method", "params"]
                }),
            )]
            .into_iter()
            .collect(),
            schema_sha256: digest.into(),
            methods: [(
                "ClientRequest".into(),
                ["thread/start".into()].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
            shapes: BTreeMap::new(),
        };
        let pinned = request(
            serde_json::json!({
                "oneOf": [
                    {"type": "number"},
                    {"type": "string", "format": "codex-path"}
                ]
            }),
            "pinned",
        );
        let desktop = request(serde_json::json!({"type": "string"}), "desktop");

        let result = compare(&pinned, &desktop);
        let delta = result
            .deltas
            .iter()
            .find(|delta| {
                delta.subject == "thread/start"
                    && delta
                        .fields
                        .iter()
                        .any(|field| field.contains("union acceptance is indeterminate"))
            })
            .expect("thread/start union must remain visible as a diagnostic");

        assert_eq!(delta.kind, DeltaKind::ShapeUnverified);
        assert!(delta.routed);
        assert_eq!(result.verdict, ProtocolVerdict::Unverified);
        assert!(result.verdict.may_arm());
        assert_eq!(result.blocking_deltas().count(), 0);
    }

    /// An added optional field is how a protocol grows. Treating it as a break
    /// is exactly the over-strictness the schema hash had.
    #[test]
    fn an_added_optional_field_is_not_a_delta() {
        let pinned = surface(
            &[],
            &[("ThreadStartResponse", &["threadId"], &["threadId"])],
            "old",
        );
        let desktop = surface(
            &[],
            &[("ThreadStartResponse", &["threadId"], &["threadId", "model"])],
            "new",
        );
        let result = compare(&pinned, &desktop);
        assert_eq!(result.verdict, ProtocolVerdict::Unverified);
        assert!(result.deltas.is_empty());
    }

    /// Direction decides severity. On a child-to-client type a newly required
    /// field is a break, because the older child will not emit it.
    #[test]
    fn a_newly_required_response_field_is_reported() {
        let pinned = surface(&[], &[("ThreadStartResponse", &[], &["threadId"])], "old");
        let desktop = surface(
            &[],
            &[("ThreadStartResponse", &["model"], &["threadId", "model"])],
            "new",
        );
        let result = compare(&pinned, &desktop);
        assert_eq!(result.deltas[0].kind, DeltaKind::FieldNewlyRequired);
        assert!(result.deltas[0].routed);
        assert_eq!(result.verdict, ProtocolVerdict::Incompatible);
    }

    /// ...and on a client-to-child type it is not, because the child ignores
    /// what it does not read.
    #[test]
    fn a_newly_required_request_field_is_not_a_delta() {
        let pinned = surface(&[], &[("ThreadStartParams", &[], &["cwd"])], "old");
        let desktop = surface(
            &[],
            &[("ThreadStartParams", &["model"], &["cwd", "model"])],
            "new",
        );
        let result = compare(&pinned, &desktop);
        assert!(result.deltas.is_empty());
    }

    #[test]
    fn a_removed_field_on_an_unrouted_type_stays_unverified() {
        let pinned = surface(
            &[],
            &[(
                "McpServerElicitationRequestParams",
                &[],
                &["elicitationId", "url"],
            )],
            "old",
        );
        let desktop = surface(
            &[],
            &[(
                "McpServerElicitationRequestParams",
                &[],
                &["requestedSchema"],
            )],
            "new",
        );
        let result = compare(&pinned, &desktop);
        assert_eq!(result.verdict, ProtocolVerdict::Unverified);
        assert_eq!(result.deltas[0].kind, DeltaKind::FieldRemoved);
        assert_eq!(result.deltas[0].fields, vec!["elicitationId", "url"]);
    }

    #[test]
    fn routed_type_names_follow_the_method_naming_convention() {
        assert!(shape_is_routed("ThreadStartParams"));
        assert!(shape_is_routed("ThreadStartResponse"));
        assert!(shape_is_routed("TurnCompletedNotification"));
        assert!(!shape_is_routed("ThreadStartedNotification"));
        assert!(!shape_is_routed("PluginReconcileParams"));
        assert_eq!(method_type_stem("thread/start"), "ThreadStart");
        assert_eq!(method_type_stem("initialize"), "Initialize");
    }

    /// The short-circuit in `compare` only means "this is the pinned
    /// combination" if this digest is the same one the lock pins and
    /// `desktop_identity` reports. Both are computed independently, over the
    /// same generated files, and this is what keeps them honest.
    #[test]
    #[ignore = "probes the installed Codex Desktop core"]
    fn hash_matches_desktop_probe() {
        let identity = crate::remote::desktop_codex::desktop_identity()
            .expect("no Codex Desktop core installed");
        let surface = ProtocolSurface::probe(Path::new(&identity.binary)).expect("probe");
        assert_eq!(surface.schema_sha256, identity.schema_sha256);
        assert!(
            surface.methods["ClientRequest"].contains("thread/start"),
            "a real Codex declares the methods the bridge routes"
        );
    }

    /// Prints the real verdict for this machine's pairing. Not an assertion
    /// about which Codex is installed — that changes under you — but the
    /// fastest way to see what a given Desktop update actually costs, and the
    /// only place the whole path runs against two real binaries.
    #[test]
    #[ignore = "probes both installed cores"]
    fn report_this_machines_pairing() {
        let data_root = crate::state::app_data_dir();
        let enhanced =
            crate::enhanced_runtime::desktop_manager::packaged_enhanced_executable(&data_root)
                .expect("no Enhanced core in this build");
        let identity = crate::remote::desktop_codex::desktop_identity()
            .expect("no Codex Desktop core installed");
        let result = compare(
            &ProtocolSurface::probe(&enhanced).expect("probe enhanced"),
            &ProtocolSurface::probe(Path::new(&identity.binary)).expect("probe desktop"),
        );
        println!("Desktop {}", identity.version);
        print_report(&result);
    }

    /// The same report for two recorded `generate-json-schema` outputs, so a
    /// pairing seen on another machine — a Mac user's Desktop against the core
    /// this release pinned — can be judged here without either binary.
    #[test]
    #[ignore = "reads VELLUM_PINNED_SCHEMA_DIR and VELLUM_DESKTOP_SCHEMA_DIR"]
    fn report_a_recorded_pairing() {
        let read = |variable: &str| {
            let root = std::env::var_os(variable)
                .unwrap_or_else(|| panic!("{variable} names a schema directory"));
            ProtocolSurface::read_dir(Path::new(&root)).expect(variable)
        };
        print_report(&compare(
            &read("VELLUM_PINNED_SCHEMA_DIR"),
            &read("VELLUM_DESKTOP_SCHEMA_DIR"),
        ));
    }

    fn print_report(result: &ProtocolCompatibility) {
        println!(
            "enhanced {} vs Desktop {}\nverdict: {}",
            result.pinned_schema_sha256,
            result.desktop_schema_sha256,
            result.verdict.as_str()
        );
        for delta in &result.deltas {
            println!(
                "  [{}] {}",
                if delta.routed { "ROUTED" } else { "     " },
                delta.describe()
            );
        }
    }

    #[test]
    fn direction_is_taken_from_the_type_suffix() {
        assert_eq!(shape_direction("ThreadStartParams"), Direction::ToChild);
        assert_eq!(shape_direction("ThreadStartResponse"), Direction::ToClient);
        assert_eq!(
            shape_direction("TurnCompletedNotification"),
            Direction::ToClient
        );
    }

    /// The envelope wins over the suffix, and an anonymous variant inherits its
    /// enclosing type. Desktop tightening `params` on its own request variants
    /// is not something the Enhanced core failed to emit.
    #[test]
    fn an_envelope_variant_keeps_the_envelopes_direction() {
        assert_eq!(shape_root("ClientRequest/oneOf[69]"), "ClientRequest");
        assert_eq!(
            shape_direction("ClientRequest/oneOf[69]"),
            Direction::ToChild
        );
        assert_eq!(
            shape_direction("ServerNotification/oneOf[3]"),
            Direction::ToClient
        );
        assert!(shape_is_routed("ThreadStartResponse/oneOf[1]"));
    }

    /// A union member is an inline object with no title and no definition
    /// entry, so a name-keyed walk misses it entirely — which is how a real
    /// wire change (elicitation swapping `elicitationId`/`url` for
    /// `requestedSchema`) read as no difference at all.
    #[test]
    fn anonymous_union_members_are_collected_by_pointer() {
        let document = serde_json::json!({
            "title": "McpServerElicitationRequestParams",
            "oneOf": [
                {"properties": {"mode": {}}, "required": ["mode"]},
                {"properties": {"elicitationId": {}, "url": {}}, "required": ["url"]}
            ],
            "definitions": {
                "Shared": {"properties": {"threadId": {}}, "required": ["threadId"]}
            }
        });
        let mut shapes = BTreeMap::new();
        collect_shapes(&document, "fallback", &mut shapes);
        assert!(
            shapes.contains_key("Shared"),
            "named definitions stay named"
        );
        let variant = shapes
            .get("McpServerElicitationRequestParams/oneOf[1]")
            .expect("the union member was not collected");
        assert!(variant.properties.contains("elicitationId"));
        assert!(variant.properties.contains("url"));
        // Descending into `definitions` under a pointer would key the same
        // shared type twice, once under a name that means nothing.
        assert!(!shapes.keys().any(|key| key.contains("definitions/Shared")));
    }
}
