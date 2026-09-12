//! Production verifying keys. An empty or placeholder key keeps live
//! auto-update disabled; fixture tests inject their own `TrustStore`.

use ed25519_dalek::VerifyingKey;

/// Compile-time hex of the production Ed25519 verifying key. Empty in
/// development and CI until the release-signing Environment is configured.
const BUNDLED_KEY_HEX: &str = match option_env!("VELLUM_UPDATE_PUBLIC_KEY") {
    Some(value) => value,
    None => "",
};

const BUNDLED_KEY_ID: &str = match option_env!("VELLUM_UPDATE_KEY_ID") {
    Some(value) => value,
    None => "vellum-updates-unconfigured",
};

#[derive(Debug, Clone)]
pub struct TrustStore {
    pub key_id: String,
    keys: Vec<(String, VerifyingKey)>,
}

impl TrustStore {
    pub fn bundled() -> Self {
        let mut store = Self {
            key_id: BUNDLED_KEY_ID.to_string(),
            keys: Vec::new(),
        };
        if let Some(key) = parse_verifying_key(BUNDLED_KEY_HEX) {
            store.keys.push((BUNDLED_KEY_ID.to_string(), key));
        }
        store
    }

    pub fn from_key(key_id: impl Into<String>, key: VerifyingKey) -> Self {
        let key_id = key_id.into();
        Self {
            key_id: key_id.clone(),
            keys: vec![(key_id, key)],
        }
    }

    pub fn live_enabled(&self) -> bool {
        !self.keys.is_empty()
    }

    pub fn key_for(&self, key_id: &str) -> Option<&VerifyingKey> {
        self.keys
            .iter()
            .find(|(id, _)| id == key_id)
            .map(|(_, key)| key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &(String, VerifyingKey)> {
        self.keys.iter()
    }
}

pub fn live_auto_update_enabled() -> bool {
    TrustStore::bundled().live_enabled()
}

pub fn parse_verifying_key(hex_key: &str) -> Option<VerifyingKey> {
    let trimmed = hex_key.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bytes = hex::decode(trimmed).ok()?;
    let array: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&array).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_trust_matches_the_compile_time_key() {
        let configured = parse_verifying_key(BUNDLED_KEY_HEX).is_some();
        assert_eq!(TrustStore::bundled().live_enabled(), configured);
        assert_eq!(live_auto_update_enabled(), configured);
    }
}
