//! Getting Codex Desktop to trust the backend relay's certificate.
//!
//! Desktop's Chromium verifies the relay against the OS trust store, so the
//! certificate goes into the *current user's* trusted roots: Windows asks
//! the user to confirm that in its own dialog, macOS asks for the login
//! password. Nothing here works around either prompt, and no machine-wide
//! store is touched.
//!
//! A refusal is remembered per certificate, so the prompt is not raised again
//! at every proxy start. Without trust the relay is simply not advertised and
//! Desktop keeps talking to chatgpt.com directly.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use vellum_proxy_runtime::desktop_backend_tls::CertificateIdentity;

const DECLINED_FILE: &str = "declined.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustOutcome {
    /// Already trusted, or the user just agreed.
    Trusted,
    /// The user refused this certificate, now or earlier.
    Declined,
    /// No supported way to install it on this platform, or the tool failed.
    Unavailable,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Declined {
    certificate_sha256: String,
    declined_at: i64,
}

/// Makes sure the OS trusts `cert_path` for `localhost`, asking the user once.
pub fn ensure_trusted(
    cert_dir: &Path,
    cert_path: &Path,
    identity: &CertificateIdentity,
) -> TrustOutcome {
    if platform::is_trusted(cert_path, identity) {
        return TrustOutcome::Trusted;
    }
    let declined_path = cert_dir.join(DECLINED_FILE);
    if declined_for(&declined_path, identity) {
        return TrustOutcome::Declined;
    }
    match platform::install(cert_path) {
        Ok(()) if platform::is_trusted(cert_path, identity) => TrustOutcome::Trusted,
        Ok(()) => {
            log::warn!("[DesktopBackend] certificate installed but still not trusted");
            TrustOutcome::Unavailable
        }
        Err(InstallError::Declined) => {
            remember_declined(&declined_path, identity);
            TrustOutcome::Declined
        }
        Err(InstallError::Unavailable(detail)) => {
            log::warn!("[DesktopBackend] cannot install relay certificate: {detail}");
            TrustOutcome::Unavailable
        }
    }
}

fn declined_for(path: &Path, identity: &CertificateIdentity) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Declined>(&bytes).ok())
        .is_some_and(|declined| declined.certificate_sha256 == identity.sha256)
}

fn remember_declined(path: &PathBuf, identity: &CertificateIdentity) {
    let record = Declined {
        certificate_sha256: identity.sha256.clone(),
        declined_at: chrono::Utc::now().timestamp(),
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
        let _ = std::fs::write(path, bytes);
    }
}

#[derive(Debug)]
enum InstallError {
    Declined,
    Unavailable(String),
}

fn quiet(mut command: Command) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window; the trust dialog is a GUI prompt of its own.
        command.creation_flags(0x08000000);
    }
    command
}

#[cfg(windows)]
mod platform {
    use super::*;

    /// `ERROR_CANCELLED`, what certutil exits with when the user says No.
    const CANCELLED: i32 = 0x800704C7_u32 as i32;

    pub fn is_trusted(_cert_path: &Path, identity: &CertificateIdentity) -> bool {
        let mut command = Command::new("certutil");
        command.args(["-user", "-store", "Root", &identity.serial_hex]);
        quiet(command)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    pub fn install(cert_path: &Path) -> Result<(), InstallError> {
        let mut command = Command::new("certutil");
        command.args(["-user", "-addstore", "Root"]).arg(cert_path);
        let output = quiet(command)
            .output()
            .map_err(|error| InstallError::Unavailable(error.to_string()))?;
        match output.status.code() {
            Some(0) => Ok(()),
            Some(CANCELLED) => Err(InstallError::Declined),
            code => Err(InstallError::Unavailable(format!(
                "certutil exited {code:?}: {}",
                String::from_utf8_lossy(&output.stdout).trim()
            ))),
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub fn is_trusted(cert_path: &Path, _identity: &CertificateIdentity) -> bool {
        let mut command = Command::new("/usr/bin/security");
        command
            .args(["verify-cert", "-L", "-p", "ssl", "-s", "localhost", "-c"])
            .arg(cert_path);
        quiet(command)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    pub fn install(cert_path: &Path) -> Result<(), InstallError> {
        let keychain = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| InstallError::Unavailable("HOME is not set".into()))?
            .join("Library/Keychains/login.keychain-db");
        let mut command = Command::new("/usr/bin/security");
        command
            .args(["add-trusted-cert", "-r", "trustRoot", "-p", "ssl", "-k"])
            .arg(keychain)
            .arg(cert_path);
        let output = quiet(command)
            .output()
            .map_err(|error| InstallError::Unavailable(error.to_string()))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        // The authorization sheet being dismissed.
        if stderr.contains("canceled") || stderr.contains("cancelled") {
            Err(InstallError::Declined)
        } else {
            Err(InstallError::Unavailable(stderr.trim().to_string()))
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;

    pub fn is_trusted(_cert_path: &Path, _identity: &CertificateIdentity) -> bool {
        false
    }

    pub fn install(_cert_path: &Path) -> Result<(), InstallError> {
        Err(InstallError::Unavailable(
            "Codex Desktop does not run on this platform".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_is_remembered_for_that_certificate_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DECLINED_FILE);
        let identity = CertificateIdentity {
            serial_hex: "01".into(),
            sha256: "aa".into(),
        };
        assert!(!declined_for(&path, &identity));
        remember_declined(&path, &identity);
        assert!(declined_for(&path, &identity));
        let replacement = CertificateIdentity {
            serial_hex: "02".into(),
            sha256: "bb".into(),
        };
        assert!(!declined_for(&path, &replacement));
    }
}
