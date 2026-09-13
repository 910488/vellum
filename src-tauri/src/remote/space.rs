//! Fail-closed free-space checks for bootstrap install/update.
//!
//! Mirrors the agent `space` module so Desktop can gate SSH artifact copies
//! without a production dependency on `vellum-remote-agent`.

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

pub fn gate_replace(free_bytes: Option<u64>, payload_bytes: u64) -> Result<(), String> {
    let Some(free_bytes) = free_bytes else {
        return Err("insufficientDiskSpace: disk free unknown; refusing replace".into());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortfall_stops_before_replace() {
        assert!(gate_replace(Some(3_000), 2_000).unwrap_err().contains("insufficientDiskSpace"));
        assert!(gate_replace(Some(6_000), 2_000).is_ok());
        assert!(gate_replace(None, 1).is_err());
    }
}
