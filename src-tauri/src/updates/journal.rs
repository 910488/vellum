//! Durable operation journal. Replaying the same operation ID is a no-op.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::machine::UpdatePhase;
use super::write_atomic;
use super::UpdateComponent;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    pub operation_id: String,
    pub component: UpdateComponent,
    pub phase: UpdatePhase,
    pub target_version: Option<String>,
    pub reason: Option<String>,
    pub host_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Journal {
    pub entries: Vec<JournalEntry>,
}

impl Journal {
    pub fn path(root: &Path) -> PathBuf {
        root.join("updates").join("journal.json")
    }

    pub fn load(root: &Path) -> Self {
        let Ok(bytes) = std::fs::read(Self::path(root)) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        write_atomic(&Self::path(root), &bytes)
    }

    pub fn upsert(&mut self, entry: JournalEntry) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|item| item.operation_id == entry.operation_id)
        {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }

    pub fn by_id(&self, operation_id: &str) -> Option<&JournalEntry> {
        self.entries
            .iter()
            .find(|entry| entry.operation_id == operation_id)
    }

    /// Remote helper: the same operation ID must not redeploy.
    pub fn already_applied(&self, operation_id: &str) -> bool {
        self.by_id(operation_id)
            .is_some_and(|entry| entry.phase == UpdatePhase::Applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_operation_id_is_updated_in_place() {
        let mut journal = Journal::default();
        journal.upsert(JournalEntry {
            operation_id: "op-1".into(),
            component: UpdateComponent::Remote,
            phase: UpdatePhase::Applying,
            target_version: Some("0.4.0".into()),
            reason: None,
            host_id: Some("h1".into()),
            created_at: 1,
            updated_at: 1,
        });
        journal.upsert(JournalEntry {
            operation_id: "op-1".into(),
            component: UpdateComponent::Remote,
            phase: UpdatePhase::Applied,
            target_version: Some("0.4.0".into()),
            reason: None,
            host_id: Some("h1".into()),
            created_at: 1,
            updated_at: 2,
        });
        assert_eq!(journal.entries.len(), 1);
        assert!(journal.already_applied("op-1"));
    }
}
