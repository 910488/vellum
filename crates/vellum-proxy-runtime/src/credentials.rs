use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use async_trait::async_trait;

#[async_trait]
pub trait CredentialProvider: Send + Sync {
    async fn get_secret(&self, id: &str) -> Result<Option<String>, String>;
    async fn list_ids(&self) -> Result<Vec<String>, String>;
}

#[derive(Debug, Default)]
pub struct MemoryCredentialProvider {
    secrets: RwLock<HashMap<String, String>>,
}

impl MemoryCredentialProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, id: impl Into<String>, value: impl Into<String>) {
        self.secrets
            .write()
            .expect("credential map poisoned")
            .insert(id.into(), value.into());
    }
}

#[async_trait]
impl CredentialProvider for MemoryCredentialProvider {
    async fn get_secret(&self, id: &str) -> Result<Option<String>, String> {
        Ok(self
            .secrets
            .read()
            .map_err(|_| "credential map poisoned".to_string())?
            .get(id)
            .cloned())
    }

    async fn list_ids(&self) -> Result<Vec<String>, String> {
        let mut ids = self
            .secrets
            .read()
            .map_err(|_| "credential map poisoned".to_string())?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        ids.sort();
        Ok(ids)
    }
}

#[derive(Debug, Clone)]
pub struct FileCredentialProvider {
    root: PathBuf,
}

impl FileCredentialProvider {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, id: &str) -> Result<PathBuf, String> {
        if id.is_empty()
            || id.contains('/')
            || id.contains('\\')
            || id.contains("..")
            || id
                .chars()
                .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
        {
            return Err(format!("invalid credential id: {id}"));
        }
        Ok(self.root.join(id))
    }
}

#[async_trait]
impl CredentialProvider for FileCredentialProvider {
    async fn get_secret(&self, id: &str) -> Result<Option<String>, String> {
        let path = self.path_for(id)?;
        if !path.exists() {
            return Ok(None);
        }
        let value = std::fs::read_to_string(&path)
            .map_err(|error| format!("failed reading secret {id}: {error}"))?;
        Ok(Some(value.trim_end_matches(['\r', '\n']).to_string()))
    }

    async fn list_ids(&self) -> Result<Vec<String>, String> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&self.root)
            .map_err(|error| format!("failed listing secrets: {error}"))?
        {
            let entry = entry.map_err(|error| format!("failed reading secret entry: {error}"))?;
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(name.to_string());
                }
            }
        }
        ids.sort();
        Ok(ids)
    }
}

pub fn secret_path_is_safe(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_credentials_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let provider = FileCredentialProvider::new(temp.path());
        std::fs::write(temp.path().join("grok"), "secret-value\n").unwrap();
        assert_eq!(
            provider.get_secret("grok").await.unwrap().as_deref(),
            Some("secret-value")
        );
        assert!(provider.get_secret("../etc/passwd").await.is_err());
    }
}
