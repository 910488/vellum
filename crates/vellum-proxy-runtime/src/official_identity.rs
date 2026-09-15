//! Stable ChatGPT login identity, separate from the selected workspace.
//!
//! `chatgpt_account_id` identifies the workspace billed by a request. Two
//! different users can hold seats in that same workspace, so it is not a
//! credential key. A credential is scoped by both the user principal and the
//! workspace.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGptIdentity {
    pub credential_id: String,
    pub workspace_id: String,
    pub email: Option<String>,
    pub workspace_name: Option<String>,
    pub plan_type: Option<String>,
    principal: String,
}

#[derive(Debug, Default, Deserialize)]
struct Claims {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_account_user_id: Option<String>,
    #[serde(default)]
    chatgpt_user_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
    #[serde(default)]
    organizations: Vec<OrganizationClaim>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaims>,
    #[serde(default, rename = "https://api.openai.com/profile")]
    profile: Option<ProfileClaims>,
}

#[derive(Debug, Default, Deserialize)]
struct AuthClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_account_user_id: Option<String>,
    #[serde(default)]
    chatgpt_user_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
    #[serde(default)]
    organizations: Vec<OrganizationClaim>,
}

#[derive(Debug, Default, Deserialize)]
struct ProfileClaims {
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OrganizationClaim {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    is_default: bool,
}

pub fn chatgpt_identity_from_jwt(token: &str) -> Option<ChatGptIdentity> {
    let payload = token.split('.').nth(1)?;
    let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    let workspace_id = claims
        .auth
        .as_ref()
        .and_then(|auth| auth.chatgpt_account_id.as_deref())
        .or(claims.chatgpt_account_id.as_deref())?
        .trim()
        .to_string();
    if workspace_id.is_empty() {
        return None;
    }
    let principal = claims
        .auth
        .as_ref()
        .and_then(|auth| auth.chatgpt_user_id.as_deref())
        .or(claims.chatgpt_user_id.as_deref())
        .or_else(|| {
            claims
                .auth
                .as_ref()
                .and_then(|auth| auth.user_id.as_deref())
        })
        .or(claims.user_id.as_deref())
        .or(claims.sub.as_deref())
        .or_else(|| {
            claims
                .auth
                .as_ref()
                .and_then(|auth| auth.chatgpt_account_user_id.as_deref())
        })
        .or(claims.chatgpt_account_user_id.as_deref())?
        .trim()
        .to_string();
    if principal.is_empty() {
        return None;
    }
    let organizations = claims
        .auth
        .as_ref()
        .map(|auth| auth.organizations.as_slice())
        .filter(|organizations| !organizations.is_empty())
        .unwrap_or(&claims.organizations);
    let workspace_name = organizations
        .iter()
        .find(|organization| organization.is_default)
        .or_else(|| organizations.first())
        .and_then(|organization| organization.title.clone())
        .filter(|title| !title.trim().is_empty());
    Some(ChatGptIdentity {
        credential_id: chatgpt_credential_id(&principal, &workspace_id),
        workspace_id,
        email: claims
            .email
            .or_else(|| claims.profile.and_then(|profile| profile.email)),
        workspace_name,
        plan_type: claims
            .auth
            .as_ref()
            .and_then(|auth| auth.chatgpt_plan_type.clone())
            .or(claims.chatgpt_plan_type),
        principal,
    })
}

pub fn chatgpt_credential_id(principal: &str, workspace_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"vellum-chatgpt-credential-v1\0");
    digest.update(principal.as_bytes());
    digest.update(b"\0");
    digest.update(workspace_id.as_bytes());
    format!("chatgpt-{}", hex::encode(digest.finalize()))
}

pub fn same_chatgpt_principal(left: &ChatGptIdentity, right: &ChatGptIdentity) -> bool {
    left.principal == right.principal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(value: serde_json::Value) -> String {
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(value.to_string())
        )
    }

    #[test]
    fn separates_two_users_in_the_same_workspace() {
        let token = |user: &str, email: &str| {
            jwt(serde_json::json!({
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "workspace-crypto",
                    "chatgpt_user_id": user,
                    "chatgpt_plan_type": "team",
                    "organizations": [{"title": "Crypto", "is_default": true}]
                },
                "https://api.openai.com/profile": {"email": email}
            }))
        };
        let jp = chatgpt_identity_from_jwt(&token("user-jp", "jp@example.test")).unwrap();
        let crypto =
            chatgpt_identity_from_jwt(&token("user-crypto", "crypto@example.test")).unwrap();
        assert_eq!(jp.workspace_id, crypto.workspace_id);
        assert_ne!(jp.credential_id, crypto.credential_id);
        assert_eq!(jp.workspace_name.as_deref(), Some("Crypto"));
    }

    #[test]
    fn separates_personal_and_workspace_for_one_user() {
        let personal = chatgpt_credential_id("user-jp", "personal");
        let workspace = chatgpt_credential_id("user-jp", "workspace-crypto");
        assert_ne!(personal, workspace);
        assert_eq!(personal, chatgpt_credential_id("user-jp", "personal"));
    }
}
