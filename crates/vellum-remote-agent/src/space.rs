//! Fail-closed free-space checks for install and update.
//!
//! Required capacity is download + extract + rollback reserve. User data is
//! never deleted to make room.

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
    }
}
