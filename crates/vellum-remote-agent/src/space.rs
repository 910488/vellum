//! Fail-closed free-space checks for install and update.
//!
//! Required capacity is download + extract + rollback reserve. User data is
//! never deleted to make room.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceRequirement {
    pub download_bytes: u64,
    pub extract_bytes: u64,
    pub rollback_reserve_bytes: u64,
}

impl SpaceRequirement {
    pub fn total(self) -> Option<u64> {
        self.download_bytes
            .checked_add(self.extract_bytes)?
            .checked_add(self.rollback_reserve_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpaceDecision {
    Allow { free_bytes: u64, required_bytes: u64 },
    Refuse { free_bytes: u64, required_bytes: u64, code: &'static str },
}

pub fn payload_replace_requirement(payload_bytes: u64) -> SpaceRequirement {
    SpaceRequirement {
        download_bytes: payload_bytes,
        extract_bytes: payload_bytes,
        rollback_reserve_bytes: payload_bytes,
    }
}

/// Fail closed when free space is unknown or below download+extract+rollback.
pub fn gate_replace(free_bytes: Option<u64>, payload_bytes: u64) -> Result<(), String> {
    let Some(free_bytes) = free_bytes else {
        return Err(
            "insufficientDiskSpace: disk free unknown; refusing replace".into(),
        );
    };
    match space_allows_replace(free_bytes, payload_replace_requirement(payload_bytes)) {
        SpaceDecision::Allow { .. } => Ok(()),
        SpaceDecision::Refuse {
            free_bytes,
            required_bytes,
            code,
        } => Err(format!(
            "{code}: need {required_bytes} bytes, have {free_bytes}"
        )),
    }
}

pub fn free_bytes_for_path(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        windows_free_bytes(path)
    }
    #[cfg(not(windows))]
    {
        unix_free_bytes(path)
    }
}

#[cfg(windows)]
fn windows_free_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            lp_directory_name: *const u16,
            lp_free_bytes_available_to_caller: *mut u64,
            lp_total_number_of_bytes: *mut u64,
            lp_total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }
    let mut dir = path.to_path_buf();
    if dir.is_file() || !dir.exists() {
        dir.pop();
    }
    if dir.as_os_str().is_empty() {
        return None;
    }
    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut available = 0_u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}

#[cfg(not(windows))]
fn unix_free_bytes(path: &Path) -> Option<u64> {
    let output = std::process::Command::new("df")
        .args(["-kP", &path.to_string_lossy()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
    let _header = lines.next()?;
    let fields: Vec<&str> = lines.next()?.split_whitespace().collect();
    let available_k: u64 = fields.get(3)?.parse().ok()?;
    Some(available_k.saturating_mul(1024))
}

pub fn space_allows_replace(free_bytes: u64, requirement: SpaceRequirement) -> SpaceDecision {
    let Some(required_bytes) = requirement.total() else {
        return SpaceDecision::Refuse {
            free_bytes,
            required_bytes: u64::MAX,
            code: "spaceRequirementOverflow",
        };
    };
    if free_bytes < required_bytes {
        SpaceDecision::Refuse {
            free_bytes,
            required_bytes,
            code: "insufficientDiskSpace",
        }
    } else {
        SpaceDecision::Allow {
            free_bytes,
            required_bytes,
        }
    }
}

pub fn space_shortfall_stops_before_replace(decision: &SpaceDecision) -> bool {
    matches!(decision, SpaceDecision::Refuse { code, .. } if *code == "insufficientDiskSpace")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortfall_stops_before_replace_and_does_not_delete_user_data() {
        let requirement = SpaceRequirement {
            download_bytes: 1_000,
            extract_bytes: 2_000,
            rollback_reserve_bytes: 1_000,
        };
        let refuse = space_allows_replace(3_000, requirement);
        assert!(space_shortfall_stops_before_replace(&refuse));
        match refuse {
            SpaceDecision::Refuse {
                free_bytes,
                required_bytes,
                code,
            } => {
                assert_eq!(free_bytes, 3_000);
                assert_eq!(required_bytes, 4_000);
                assert_eq!(code, "insufficientDiskSpace");
            }
            SpaceDecision::Allow { .. } => panic!("shortfall must refuse"),
        }
        let allow = space_allows_replace(4_000, requirement);
        assert!(matches!(
            allow,
            SpaceDecision::Allow {
                free_bytes: 4_000,
                required_bytes: 4_000
            }
        ));
        assert!(gate_replace(None, 1).unwrap_err().contains("insufficientDiskSpace"));
        assert!(gate_replace(Some(5_999), 2_000).unwrap_err().contains("insufficientDiskSpace"));
        assert!(gate_replace(Some(6_000), 2_000).is_ok());
        let temp = tempfile::tempdir().unwrap();
        assert!(free_bytes_for_path(temp.path()).is_some_and(|bytes| bytes > 0));
    }
}
