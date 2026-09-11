use std::process::Command;

use crate::protocol::{CodexInventory, DockerInventory, HostCapabilities, SystemInventory};

pub fn probe_host() -> HostCapabilities {
    let docker = probe_docker();
    let codex = probe_codex();
    HostCapabilities {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        docker_available: docker.available,
        docker_mode: docker.mode,
        rootless_docker: docker.rootless,
        user_systemd_available: user_systemd_available(),
        linger_enabled: linger_enabled(),
        codex_binary: codex.binary,
        codex_version: codex.version,
    }
}

struct DockerProbe {
    available: bool,
    mode: String,
    rootless: bool,
}

struct CodexProbe {
    binary: Option<String>,
    version: Option<String>,
}

fn probe_docker() -> DockerProbe {
    let output = Command::new("docker")
        .arg("version")
        .arg("--format")
        .arg("{{.Server.Os}}/{{.Server.Arch}}")
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let info = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let rootless = Command::new("docker")
                .args(["info", "--format", "{{.SecurityOptions}}"])
                .output()
                .ok()
                .map(|out| String::from_utf8_lossy(&out.stdout).contains("rootless"))
                .unwrap_or(false);
            DockerProbe {
                available: true,
                mode: if rootless {
                    format!("rootless:{info}")
                } else {
                    format!("system:{info}")
                },
                rootless,
            }
        }
        _ => DockerProbe {
            available: false,
            mode: "unavailable".into(),
            rootless: false,
        },
    }
}

fn probe_codex() -> CodexProbe {
    let native = crate::native_codex::discover_native().ok();
    probe_codex_with_native(native.as_ref())
}

/// Same probe, but reusing a `discover_native()` result the caller already
/// has (e.g. one shared call in `host.managerSnapshot`) instead of spawning
/// another `codex --version`/`codex app-server daemon version` pair.
fn probe_codex_with_native(
    native: Option<&crate::native_codex::NativeCodexRuntimeStatus>,
) -> CodexProbe {
    if let Some(native) = native {
        if native.codex_binary.is_some() || native.codex_version.is_some() {
            return CodexProbe {
                binary: native.codex_binary.clone(),
                version: native.codex_version.clone(),
            };
        }
    }
    let override_candidate = std::env::var("VELLUM_CODEX_BIN").ok();
    for candidate in override_candidate
        .as_deref()
        .into_iter()
        .chain(["codex", "codex.exe"])
    {
        let output = Command::new(candidate).arg("--version").output();
        if let Ok(output) = output {
            if output.status.success() {
                let version = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                return CodexProbe {
                    binary: Some(candidate.to_string()),
                    version: if version.is_empty() {
                        None
                    } else {
                        Some(version)
                    },
                };
            }
        }
    }
    CodexProbe {
        binary: None,
        version: None,
    }
}

/// M30: CPU / memory / disk / hostname facts for the inventory V2 surface.
/// Linux-only probes degrade to `None` on other platforms; the desktop never
/// fails hard on a missing field.
pub fn probe_system_inventory() -> SystemInventory {
    let disk = probe_disk_bytes();
    SystemInventory {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        hostname: read_hostname(),
        cpu_cores: probe_cpu_cores(),
        memory_bytes: probe_memory_bytes(),
        disk_total_bytes: disk.map(|(total, _)| total),
        disk_free_bytes: disk.map(|(_, free)| free),
    }
}

/// M30: Docker version / daemon / context / permission facts.
pub fn probe_docker_inventory() -> DockerInventory {
    let basic = probe_docker();
    if !basic.available {
        return DockerInventory {
            available: false,
            mode: basic.mode,
            ..Default::default()
        };
    }
    DockerInventory {
        available: true,
        mode: basic.mode,
        rootless: basic.rootless,
        daemon: docker_version_field("{{.Server.Os}}/{{.Server.Arch}}"),
        server_version: docker_version_field("{{.Server.Version}}"),
        client_version: docker_version_field("{{.Client.Version}}"),
        context: command_first_line("docker", &["context", "show"]),
        user_in_docker_group: user_in_group("docker"),
    }
}

/// M30: Codex binary source, CODEX_HOME and app-server compatibility facts.
pub fn probe_codex_inventory() -> CodexInventory {
    let native = crate::native_codex::discover_native().ok();
    probe_codex_inventory_with_native(native.as_ref())
}

/// Same inventory, reusing a `discover_native()` result the caller already
/// has. See [`probe_codex_with_native`].
pub fn probe_codex_inventory_with_native(
    native: Option<&crate::native_codex::NativeCodexRuntimeStatus>,
) -> CodexInventory {
    if let Some(status) = native {
        if status.codex_binary.is_some() || status.codex_version.is_some() {
            return CodexInventory {
                binary: status.codex_binary.clone(),
                version: status.codex_version.clone(),
                source: "native".into(),
                codex_home: Some(status.codex_home.clone()),
                standalone_installed: status.standalone_installed,
                app_cli_discoverable: status.cli_launcher.ready,
                app_cli_path: status.cli_launcher.path.clone(),
                app_server_supported: status.daemon_version.is_some(),
                compatible: status.compatible,
                compatibility_reason: status.compatibility_reason.clone(),
            };
        }
    }
    let path_probe = probe_codex_with_native(native);
    if let Some(binary) = path_probe.binary {
        return CodexInventory {
            binary: Some(binary),
            version: path_probe.version,
            source: "path".into(),
            codex_home: native.map(|status| status.codex_home.clone()),
            standalone_installed: native
                .map(|status| status.standalone_installed)
                .unwrap_or(false),
            app_cli_discoverable: native.is_some_and(|status| status.cli_launcher.ready),
            app_cli_path: native.and_then(|status| status.cli_launcher.path.clone()),
            app_server_supported: false,
            compatible: native.map(|status| status.compatible).unwrap_or(false),
            compatibility_reason: native.and_then(|status| status.compatibility_reason.clone()),
        };
    }
    CodexInventory {
        source: "missing".into(),
        ..Default::default()
    }
}

fn probe_cpu_cores() -> Option<u64> {
    if let Ok(output) = Command::new("nproc").output() {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(cores) = stdout.trim().parse::<u64>().ok().filter(|value| *value > 0) {
                return Some(cores);
            }
        }
    }
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|input| parse_cpu_count(&input))
}

fn probe_memory_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|input| parse_meminfo_total(&input))
}

fn probe_disk_bytes() -> Option<(u64, u64)> {
    let output = Command::new("df").args(["-P", "-B1", "/"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_df_bytes(&String::from_utf8_lossy(&output.stdout))
}

fn read_hostname() -> Option<String> {
    let output = Command::new("hostname").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn docker_version_field(field: &str) -> Option<String> {
    let output = Command::new("docker")
        .args(["version", "--format", field])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn command_first_line(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn user_in_group(group: &str) -> Option<bool> {
    let output = Command::new("id").arg("-nG").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let groups = String::from_utf8_lossy(&output.stdout);
    Some(groups.split_whitespace().any(|item| item == group))
}

/// Parse `MemTotal: <kib> kB` from `/proc/meminfo`, returning bytes.
fn parse_meminfo_total(input: &str) -> Option<u64> {
    let line = input.lines().find(|line| line.starts_with("MemTotal:"))?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 2 {
        return None;
    }
    let kib: u64 = fields[1].parse().ok()?;
    kib.checked_mul(1024)
}

/// Parse `df -P -B1 /` output into (total, available) bytes.
fn parse_df_bytes(input: &str) -> Option<(u64, u64)> {
    let mut lines = input.lines().filter(|line| !line.trim().is_empty());
    let _header = lines.next()?;
    let fields: Vec<&str> = lines.next()?.split_whitespace().collect();
    if fields.len() < 4 {
        return None;
    }
    let total: u64 = fields[1].parse().ok()?;
    let available: u64 = fields[3].parse().ok()?;
    Some((total, available))
}

/// Count `processor` lines from `/proc/cpuinfo`.
fn parse_cpu_count(input: &str) -> Option<u64> {
    let count = input
        .lines()
        .filter(|line| line.trim().starts_with("processor"))
        .count();
    (count > 0).then_some(count as u64)
}

fn user_systemd_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn linger_enabled() -> bool {
    let user = Command::new("id")
        .arg("-un")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    let Some(user) = user else { return false };
    Command::new("loginctl")
        .args(["show-user", &user, "-p", "Linger", "--value"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_codex_is_reported_without_fabricating_a_version() {
        let probe = probe_codex();
        assert_eq!(probe.binary.is_some(), probe.version.is_some());
    }

    #[test]
    fn meminfo_total_is_parsed_as_bytes() {
        let sample = "\
MemTotal:       16384000 kB
MemFree:         1000000 kB
MemAvailable:   8000000 kB
";
        assert_eq!(parse_meminfo_total(sample), Some(16_777_216_000));
        assert_eq!(parse_meminfo_total("MemFree: 100 kB"), None);
        assert_eq!(parse_meminfo_total("no meminfo here"), None);
        assert_eq!(parse_meminfo_total("MemTotal: nope kB"), None);
    }

    #[test]
    fn df_posix_output_is_parsed_into_total_and_available() {
        let sample = "\
Filesystem 1B-blocks Used Available Capacity Mounted on
/dev/root 123456789012 34567890123 88888999889 39% /
";
        assert_eq!(
            parse_df_bytes(sample),
            Some((123_456_789_012, 88_888_999_889))
        );
        assert_eq!(parse_df_bytes("only header"), None);
        assert_eq!(parse_df_bytes(""), None);
        let bad = "Filesystem 1B-blocks Used Available Capacity Mounted on\n/dev/root x y z 0% /\n";
        assert_eq!(parse_df_bytes(bad), None);
    }

    #[test]
    fn cpuinfo_processor_lines_are_counted() {
        let sample = "\
processor\t: 0
model name\t: ARMv8
processor\t: 1
model name\t: ARMv8
processor\t: 2
";
        assert_eq!(parse_cpu_count(sample), Some(3));
        assert_eq!(parse_cpu_count("no processors here"), None);
    }
}
