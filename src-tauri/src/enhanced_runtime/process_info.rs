//! Minimal process identity helpers.
//!
//! The bridge attestation has to prove which process adopted it, and the
//! qualification gates have to reject a stale attestation left behind by a
//! bridge that already exited. Both need process facts, not agent logic.

use std::path::PathBuf;

/// True when a process with this id currently exists.
pub fn pid_is_alive(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        windows::pid_is_alive(pid)
    }
    #[cfg(not(target_os = "windows"))]
    {
        unix::pid_is_alive(pid)
    }
}

pub fn parent_pid(pid: u32) -> Option<u32> {
    #[cfg(target_os = "windows")]
    {
        windows::parent_pid(pid)
    }
    #[cfg(not(target_os = "windows"))]
    {
        unix::parent_pid(pid)
    }
}

pub fn executable_of(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        windows::executable_of(pid)
    }
    #[cfg(not(target_os = "windows"))]
    {
        unix::executable_of(pid)
    }
}

/// A live process image. Used to name leftover Vellum hosts that still own
/// the local Proxy port or Codex config lease after this process stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessImage {
    pub pid: u32,
    pub executable: PathBuf,
}

/// True when `path` is a Vellum Desktop / Proxy host, not a sidecar or Codex.
pub fn is_vellum_host_executable(path: &std::path::Path) -> bool {
    let name = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(name.as_str(), "vellum-proxy-desktop" | "vellum")
}

/// Other Vellum host processes still running for this user.
///
/// Closing this window is not enough if an installed `vellum-proxy-desktop`
/// started from Explorer still holds 15721 and the Codex config lease. That
/// leftover is the live account injector, so a switch in this process is not.
pub fn leftover_vellum_hosts() -> Vec<ProcessImage> {
    let self_pid = std::process::id();
    let self_exe = std::env::current_exe().ok();
    process_images()
        .into_iter()
        .filter(|image| {
            image.pid != self_pid
                && is_vellum_host_executable(&image.executable)
                && self_exe
                    .as_ref()
                    .is_none_or(|ours| canonical(&image.executable) != canonical(ours))
        })
        .collect()
}

pub fn format_process_images(images: &[ProcessImage]) -> String {
    images
        .iter()
        .map(|image| format!("pid {} ({})", image.pid, image.executable.display()))
        .collect::<Vec<_>>()
        .join("; ")
}

fn process_images() -> Vec<ProcessImage> {
    #[cfg(target_os = "windows")]
    {
        windows::process_images()
    }
    #[cfg(not(target_os = "windows"))]
    {
        unix::process_images()
    }
}

fn canonical(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(target_os = "windows")]
mod windows {
    use std::mem::size_of;
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    pub fn pid_is_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            CloseHandle(handle);
        }
        true
    }

    pub fn parent_pid(pid: u32) -> Option<u32> {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            let mut parent = None;
            if Process32FirstW(snapshot, &mut entry) != 0 {
                loop {
                    if entry.th32ProcessID == pid {
                        parent = Some(entry.th32ParentProcessID);
                        break;
                    }
                    if Process32NextW(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
            parent
        }
    }

    pub fn executable_of(pid: u32) -> Option<PathBuf> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let mut buffer = vec![0_u16; 32_768];
            let mut length = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) != 0;
            CloseHandle(handle);
            ok.then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize])))
        }
    }

    pub fn process_images() -> Vec<super::ProcessImage> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut images = Vec::new();
        unsafe {
            if Process32FirstW(snapshot, &mut entry) != 0 {
                loop {
                    let name_len = entry
                        .szExeFile
                        .iter()
                        .position(|&unit| unit == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
                    if super::is_vellum_host_executable(std::path::Path::new(&name)) {
                        if let Some(executable) = executable_of(entry.th32ProcessID) {
                            images.push(super::ProcessImage {
                                pid: entry.th32ProcessID,
                                executable,
                            });
                        }
                    }
                    if Process32NextW(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
        }
        images
    }
}

#[cfg(not(target_os = "windows"))]
mod unix {
    use std::path::PathBuf;

    pub fn pid_is_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    pub fn parent_pid(pid: u32) -> Option<u32> {
        ps_field(pid, "ppid=").and_then(|value| value.parse().ok())
    }

    pub fn executable_of(pid: u32) -> Option<PathBuf> {
        ps_field(pid, "comm=")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    }

    fn ps_field(pid: u32, format: &str) -> Option<String> {
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", format])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty())
    }

    pub fn process_images() -> Vec<super::ProcessImage> {
        let output = std::process::Command::new("ps")
            .args(["-axo", "pid=,comm="])
            .output()
            .ok();
        let Some(output) = output.filter(|output| output.status.success()) else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let (pid, rest) = line.split_once(char::is_whitespace)?;
                let pid = pid.parse::<u32>().ok()?;
                let comm = rest.trim();
                let named = std::path::PathBuf::from(comm);
                if !super::is_vellum_host_executable(&named) {
                    return None;
                }
                Some(super::ProcessImage {
                    pid,
                    executable: executable_of(pid).unwrap_or(named),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive_and_pid_zero_is_not() {
        assert!(pid_is_alive(std::process::id()));
        assert!(!pid_is_alive(0));
    }

    #[test]
    fn vellum_host_names_do_not_include_the_bridge_sidecar() {
        assert!(is_vellum_host_executable(std::path::Path::new(
            r"C:\Users\developer\AppData\Local\Vellum\vellum-proxy-desktop.exe"
        )));
        assert!(is_vellum_host_executable(std::path::Path::new(
            "/Applications/Vellum.app/Contents/MacOS/vellum"
        )));
        assert!(!is_vellum_host_executable(std::path::Path::new(
            r"C:\vellum\vellum-codex-app-server.exe"
        )));
        assert!(!is_vellum_host_executable(std::path::Path::new(
            r"C:\Users\developer\AppData\Local\OpenAI\Codex\bin\hash\codex.exe"
        )));
    }
}
