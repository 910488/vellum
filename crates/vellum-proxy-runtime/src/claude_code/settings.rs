//! Pure three-way settings lease. The host owns locking and atomic persistence.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
pub struct SettingsLease {
    original: BTreeMap<String, Option<Value>>,
    applied: BTreeMap<String, Value>,
    env_existed: bool,
}

impl SettingsLease {
    pub fn apply(settings: &mut Value, proxy: &str, ca: &str) -> Result<Self, String> {
        let root = settings
            .as_object_mut()
            .ok_or("Claude settings must be an object")?;
        let env_existed = root.contains_key("env");
        let env = root
            .entry("env")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or("Claude settings env must be an object")?;
        // Lowercase wins in Claude Code. Refuse every existing proxy/CA override:
        // silently breaking a corporate chain is not an acceptable installation.
        for key in [
            "https_proxy",
            "HTTPS_PROXY",
            "http_proxy",
            "HTTP_PROXY",
            "no_proxy",
            "NO_PROXY",
            "NODE_EXTRA_CA_CERTS",
            "NODE_TLS_REJECT_UNAUTHORIZED",
        ] {
            if env.contains_key(key) {
                return Err(format!(
                    "Existing {key} setting conflicts with Claude Code routing"
                ));
            }
        }
        let applied: BTreeMap<String, Value> = BTreeMap::from([
            ("HTTPS_PROXY".into(), Value::String(proxy.into())),
            ("NODE_EXTRA_CA_CERTS".into(), Value::String(ca.into())),
        ]);
        let original = applied
            .keys()
            .map(|key| (key.clone(), env.get(key).cloned()))
            .collect();
        env.extend(applied.clone());
        Ok(Self {
            original,
            applied,
            env_existed,
        })
    }

    /// Restore owned fields only; report externally edited fields to the host.
    pub fn restore(&self, settings: &mut Value) -> Result<Vec<String>, String> {
        let root = settings
            .as_object_mut()
            .ok_or("Claude settings must be an object")?;
        let Some(env) = root.get_mut("env") else {
            return Ok(vec![]);
        };
        let env = env
            .as_object_mut()
            .ok_or("Claude env was replaced; repair it manually")?;
        let mut conflicts = vec![];
        for (key, applied) in &self.applied {
            if env.get(key) == Some(applied) {
                if let Some(Some(value)) = self.original.get(key) {
                    env.insert(key.clone(), value.clone());
                } else {
                    env.remove(key);
                }
            } else if env.contains_key(key) {
                conflicts.push(key.clone());
            }
        }
        if !self.env_existed && env.is_empty() {
            root.remove("env");
        }
        Ok(conflicts)
    }

    pub fn matches(&self, settings: &Value) -> bool {
        self.applied
            .iter()
            .all(|(key, value)| settings.get("env").and_then(|e| e.get(key)) == Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn restore_keeps_concurrent_user_changes() {
        let mut value = json!({"permissions":{"allow":["Read"]}});
        let lease = SettingsLease::apply(&mut value, "http://local", "ca.pem").unwrap();
        value["env"]["HTTPS_PROXY"] = json!("http://company");
        value["env"]["MY_SETTING"] = json!("keep");
        assert_eq!(lease.restore(&mut value).unwrap(), vec!["HTTPS_PROXY"]);
        assert_eq!(
            value["env"],
            json!({"HTTPS_PROXY":"http://company","MY_SETTING":"keep"})
        );
        assert_eq!(value["permissions"], json!({"allow":["Read"]}));
    }
    #[test]
    fn complete_round_trip_and_proxy_conflicts() {
        let initial = json!({"model":"original"});
        let mut value = initial.clone();
        let lease = SettingsLease::apply(&mut value, "proxy", "ca").unwrap();
        assert!(lease.matches(&value));
        assert!(SettingsLease::apply(&mut value, "other", "ca").is_err());
        lease.restore(&mut value).unwrap();
        assert_eq!(value, initial);
        assert!(
            SettingsLease::apply(&mut json!({"env":{"https_proxy":"company"}}), "proxy", "ca")
                .is_err()
        );
    }
}
