//! Production verifying keys. Fixture tests inject their own `TrustStore`.
//!
//! The verifying key is public, so release builds carry the production key
//! even when the build environment does not set one. Local `build:main` and
//! `build:mac` builds used to come out without it, and a build without the key
//! can never update itself: its users had to find and install a new version by
//! hand. Debug builds stay off unless the environment opts in, so `tauri dev`
//! never stages an installer and runs it on exit.

use ed25519_dalek::VerifyingKey;

/// Ed25519 key behind `secrets.VELLUM_UPDATE_SIGNING_KEY`; the same value
/// as the repository variable `VELLUM_UPDATE_PUBLIC_KEY`.
const PRODUCTION_KEY_HEX: &str = "64f2f1d3d9f482f0194b8bcb63ce0ec4c3168f2ccda81617ad36e4f9dd67dd27";
const PRODUCTION_KEY_ID: &str = "vellum-updates-2026-09";

const fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    match value {
        Some(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

const BUNDLED_KEY_HEX: &str = match non_empty(option_env!("VELLUM_UPDATE_PUBLIC_KEY")) {
    Some(value) => value,
    None if cfg!(debug_assertions) => "",
    None => PRODUCTION_KEY_HEX,
};

const BUNDLED_KEY_ID: &str = match non_empty(option_env!("VELLUM_UPDATE_KEY_ID")) {
    Some(value) => value,
    None => PRODUCTION_KEY_ID,
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
    fn production_key_is_a_valid_verifying_key() {
        // A typo here would silently turn auto-update off in every release.
        assert!(parse_verifying_key(PRODUCTION_KEY_HEX).is_some());
    }

    #[test]
    fn bundled_trust_matches_the_compile_time_key() {
        let configured = parse_verifying_key(BUNDLED_KEY_HEX).is_some();
        assert_eq!(TrustStore::bundled().live_enabled(), configured);
        assert_eq!(live_auto_update_enabled(), configured);
    }
}
