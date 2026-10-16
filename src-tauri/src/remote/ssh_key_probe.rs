//! Untrusted host-key collection. Windows bypasses ssh-keyscan's KEX regression.
use super::*;

pub(super) fn fetch(
    root: &Path,
    destination: &str,
    target: &SshTarget,
    windows: bool,
) -> AppResult<Vec<PendingHostFingerprint>> {
    fetch_with(
        target,
        windows,
        || run_keyscan(&crate::process::resolve_ssh_tool("ssh-keyscan"), target),
        || {
            probe_with(root, destination, target, |args| {
                super::super::ssh_added::command(root, destination)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .output()
            })
        },
    )
}

fn fetch_with(
    target: &SshTarget,
    windows: bool,
    scan: impl FnOnce() -> std::io::Result<Output>,
    kex: impl FnOnce() -> AppResult<Vec<PendingHostFingerprint>>,
) -> AppResult<Vec<PendingHostFingerprint>> {
    // Windows never starts a scanner. Elsewhere launch failure or unsupported
    // KEX falls back once; ordinary connection errors do not cause a burst.
    let scanned = if windows { None } else { scan().ok() };
    if let Some(output) = scanned {
        let keys = parse_keyscan_output(target, &output.stdout);
        if !keys.is_empty() {
            return Ok(keys);
        }
        if !unsupported_kex(&output.stderr) {
            return Err(AppError::Message(keyscan_diagnostic(
                Path::new("ssh-keyscan"),
                &output,
            )));
        }
    }
    kex()
}

fn unsupported_kex(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr).contains("unsupported KEX")
}

fn probe_args(destination: &str, known: &Path, global: &Path) -> Vec<String> {
    let mut args = vec![];
    for option in [
        "BatchMode=yes",
        "StrictHostKeyChecking=accept-new",
        "PreferredAuthentications=none",
        "PubkeyAuthentication=no",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
        "GSSAPIAuthentication=no",
        "HostbasedAuthentication=no",
        "ConnectTimeout=5",
        "ConnectionAttempts=1",
        "ControlPath=none",
        "ControlMaster=no",
        "PermitLocalCommand=no",
        "ClearAllForwardings=yes",
        "ForwardAgent=no",
        "RequestTTY=no",
        "RemoteCommand=none",
        "KnownHostsCommand=none",
        "VerifyHostKeyDNS=no",
    ] {
        args.extend(["-o".into(), option.into()]);
    }
    args.extend(["-o".into(), known_hosts_option(known)]);
    args.extend([
        "-o".into(),
        known_hosts_option(global).replacen("UserKnownHostsFile", "GlobalKnownHostsFile", 1),
    ]);
    args.extend(["--".into(), destination.into(), "exit".into()]);
    args
}

fn probe_with(
    root: &Path,
    destination: &str,
    target: &SshTarget,
    run: impl FnOnce(&[String]) -> std::io::Result<Output>,
) -> AppResult<Vec<PendingHostFingerprint>> {
    let parent = root.join("temp/ssh-host-key");
    fs::create_dir_all(&parent).map_err(|e| AppError::Message(e.to_string()))?;
    // RAII cleanup includes launch failure, authentication rejection and parse failure.
    let temp = tempfile::Builder::new()
        .prefix("probe-")
        .tempdir_in(parent)
        .map_err(|e| AppError::Message(e.to_string()))?;
    let known = temp.path().join("known_hosts");
    let global = temp.path().join("empty_global");
    fs::write(&global, []).map_err(|e| AppError::Message(e.to_string()))?;
    let output = run(&probe_args(destination, &known, &global))
        .map_err(|e| AppError::Message(format!("SSH host key probe unavailable: {e}")))?;
    // Authentication failure is expected. Only the captured key is meaningful.
    let bytes = fs::read(&known).unwrap_or_default();
    let keys = parse_keyscan_output(target, &bytes);
    if keys.is_empty() {
        return Err(AppError::Message(keyscan_diagnostic(
            Path::new("ssh"),
            &output,
        )));
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn native_probe_options_preserve_effective_config_without_connecting() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data with spaces");
        let alias = "vellum-added-probe";
        let config = super::super::super::ssh_added::config_path(&root);
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(config, "Host vellum-added-probe\n HostName resolved.example\n Port 2222\n User fixture-user\n HostKeyAlias original-trust-alias\n ProxyJump fixture-jump\n").unwrap();
        let target = SshTarget {
            host: "resolved.example".into(),
            port: 2222,
        };
        let result = probe_with(&root, alias, &target, |args| {
            // -G only expands config; neither target nor jump host is contacted.
            let output = super::super::super::ssh_added::command(&root, alias)
                .arg("-G")
                .args(args)
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let expanded = String::from_utf8_lossy(&output.stdout);
            for expected in [
                "hostname resolved.example",
                "port 2222",
                "user fixture-user",
                "hostkeyalias original-trust-alias",
                "proxyjump fixture-jump",
                "preferredauthentications none",
            ] {
                assert!(
                    expanded.contains(expected),
                    "missing {expected}: {expanded}"
                );
            }
            assert!(expanded.contains("data with spaces"));
            // OpenSSH omits ControlPath entirely when it is disabled.
            assert!(!expanded
                .lines()
                .any(|line| line.starts_with("controlpath ") && line != "controlpath none"));
            Ok(output)
        });
        assert!(
            result.is_err(),
            "config expansion cannot collect or trust a host key"
        );
        assert!(!known_hosts_path(&root).exists());
        assert_eq!(
            fs::read_dir(root.join("temp/ssh-host-key"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn unsupported_kex_is_the_only_scanner_protocol_fallback() {
        assert!(unsupported_kex(b"choose_kex: unsupported KEX\n"));
        assert!(!unsupported_kex(b"Connection timed out"));
    }

    #[test]
    fn windows_skips_keyscan_and_other_platforms_fall_back_only_when_needed() {
        let target = SshTarget {
            host: "example".into(),
            port: 22,
        };
        for (windows, stderr, expected) in [
            (true, "unused", vec!["kex"]),
            (false, "valid", vec!["keyscan"]),
            (false, "launch", vec!["keyscan", "kex"]),
            (false, "choose_kex: unsupported KEX", vec!["keyscan", "kex"]),
            (false, "Connection timed out", vec!["keyscan"]),
        ] {
            let calls = std::cell::RefCell::new(vec![]);
            let _ = fetch_with(
                &target,
                windows,
                || {
                    calls.borrow_mut().push("keyscan");
                    if stderr == "launch" {
                        return Err(std::io::Error::other("scanner missing"));
                    }
                    Ok(Output {
                        status: failed_status(),
                        stdout: if stderr == "valid" {
                            b"example ssh-ed25519 AAAA\n".to_vec()
                        } else {
                            vec![]
                        },
                        stderr: stderr.as_bytes().into(),
                    })
                },
                || {
                    calls.borrow_mut().push("kex");
                    Ok(vec![])
                },
            );
            assert_eq!(*calls.borrow(), expected);
        }
    }

    #[test]
    fn native_probe_preserves_alias_config_and_removes_temporary_files_on_all_exits() {
        let root = tempfile::tempdir().unwrap();
        let target = SshTarget {
            host: "resolved.example".into(),
            port: 2222,
        };
        for mode in ["key", "empty", "launch"] {
            let result = probe_with(root.path(), "original-alias", &target, |args| {
                assert!(!args.iter().any(|a| a == "-F"));
                assert_eq!(&args[args.len() - 3..], &["--", "original-alias", "exit"]);
                assert!(args.iter().any(|a| a == "PreferredAuthentications=none"));
                assert!(args.iter().any(|a| a == "ControlPath=none"));
                let dirs = fs::read_dir(root.path().join("temp/ssh-host-key"))?
                    .collect::<Result<Vec<_>, _>>()?;
                assert_eq!(dirs.len(), 1);
                if mode == "launch" {
                    return Err(std::io::Error::other("injected"));
                }
                if mode == "key" {
                    fs::write(
                        dirs[0].path().join("known_hosts"),
                        "alias ssh-ed25519 AAAA\n",
                    )?;
                }
                Ok(Output {
                    status: failed_status(),
                    stdout: vec![],
                    stderr: b"Permission denied".to_vec(),
                })
            });
            assert_eq!(result.is_ok(), mode == "key");
            if let Ok(keys) = result {
                assert!(keys[0].key_line.starts_with("[resolved.example]:2222 "));
            }
            assert_eq!(
                fs::read_dir(root.path().join("temp/ssh-host-key"))
                    .unwrap()
                    .count(),
                0
            );
            assert!(!known_hosts_path(root.path()).exists());
        }
    }

    fn failed_status() -> std::process::ExitStatus {
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(255)
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(255 << 8)
        }
    }
}
