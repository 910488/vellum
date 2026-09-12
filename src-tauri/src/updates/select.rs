//! Highest compatible version, not GitHub `/latest` and not publish time.

use serde::{Deserialize, Serialize};

use super::manifest::{parse_version, UpdateManifest};
use super::UpdateComponent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Channel {
    Stable,
    Preview,
}

impl Channel {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "stable" => Some(Self::Stable),
            "preview" => Some(Self::Preview),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Preview => "preview",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReleaseCandidate {
    pub manifest: UpdateManifest,
    pub github_prerelease: bool,
    /// Present for diagnostics only. Selection must not use this.
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub version: String,
    pub release_tag: String,
    pub sequence: u64,
}

#[derive(Debug, Clone)]
pub struct SelectContext<'a> {
    pub component: UpdateComponent,
    pub channel: Channel,
    pub current_version: &'a str,
    pub platform: &'a str,
    pub arch: &'a str,
    pub installed_desktop: &'a str,
    pub bridge_api: &'a str,
    pub remote_protocol: &'a str,
}

pub fn select_compatible(
    candidates: &[ReleaseCandidate],
    ctx: &SelectContext<'_>,
) -> Option<Selection> {
    let current = parse_version(ctx.current_version);
    let mut best: Option<(semver::Version, Selection)> = None;
    for candidate in candidates {
        if candidate.manifest.component != ctx.component {
            continue;
        }
        if ctx.channel == Channel::Stable
            && (candidate.github_prerelease || candidate.manifest.prerelease)
        {
            continue;
        }
        if !candidate.manifest.min_desktop_version.is_empty()
            && parse_version(ctx.installed_desktop)
                .zip(parse_version(&candidate.manifest.min_desktop_version))
                .is_none_or(|(installed, min)| installed < min)
        {
            continue;
        }
        if !candidate.manifest.bridge_api_compat.matches(ctx.bridge_api) {
            continue;
        }
        if !candidate
            .manifest
            .remote_protocol_compat
            .matches(ctx.remote_protocol)
        {
            continue;
        }
        if candidate.manifest.data_format.irreversible_migration {
            continue;
        }
        let Some(asset) = candidate
            .manifest
            .assets
            .iter()
            .find(|asset| asset.platform == ctx.platform && asset.arch == ctx.arch)
        else {
            continue;
        };
        let _ = asset;
        let Some(version) = parse_version(&candidate.manifest.version) else {
            continue;
        };
        if let Some(current) = &current {
            if version <= *current {
                continue;
            }
        }
        let selection = Selection {
            version: candidate.manifest.version.clone(),
            release_tag: candidate.manifest.release_tag.clone(),
            sequence: candidate.manifest.sequence,
        };
        match &best {
            None => best = Some((version, selection)),
            Some((best_version, _)) if version > *best_version => {
                best = Some((version, selection));
            }
            Some(_) => {}
        }
    }
    best.map(|(_, selection)| selection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updates::manifest::{AssetRef, CompatRange, DataFormat, MANIFEST_SCHEMA_VERSION};

    fn manifest(component: UpdateComponent, version: &str, prerelease: bool) -> UpdateManifest {
        UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            component,
            version: version.into(),
            source_commit: "c".into(),
            release_tag: format!("{}{version}", component.tag_prefix()),
            sequence: 1,
            key_id: "k".into(),
            prerelease,
            release_notes: None,
            min_desktop_version: "0.1.0".into(),
            bridge_api_compat: CompatRange::new("*"),
            remote_protocol_compat: CompatRange::new("*"),
            data_format: DataFormat::default(),
            assets: vec![AssetRef {
                platform: "windows".into(),
                arch: "x64".into(),
                name: format!("{version}.bin"),
                size: 1,
                sha256: "aa".into(),
            }],
            core: None,
        }
    }

    fn ctx(channel: Channel, current: &'static str) -> SelectContext<'static> {
        SelectContext {
            component: UpdateComponent::Desktop,
            channel,
            current_version: current,
            platform: "windows",
            arch: "x64",
            installed_desktop: "0.2.9",
            bridge_api: "1.0.0",
            remote_protocol: "3",
        }
    }

    #[test]
    fn stable_ignores_prerelease_and_picks_highest_compatible() {
        let candidates = [
            ReleaseCandidate {
                manifest: manifest(UpdateComponent::Desktop, "0.3.0-rc.1", true),
                github_prerelease: true,
                published_at: Some("2026-09-12T00:00:00Z".into()),
            },
            ReleaseCandidate {
                manifest: manifest(UpdateComponent::Desktop, "0.2.8", false),
                github_prerelease: false,
                published_at: Some("2026-09-13T00:00:00Z".into()),
            },
            ReleaseCandidate {
                manifest: manifest(UpdateComponent::Desktop, "0.3.0", false),
                github_prerelease: false,
                published_at: Some("2026-09-01T00:00:00Z".into()),
            },
        ];
        let picked = select_compatible(&candidates, &ctx(Channel::Stable, "0.2.9")).unwrap();
        assert_eq!(picked.version, "0.3.0");
    }

    #[test]
    fn preview_may_take_rc() {
        let candidates = [ReleaseCandidate {
            manifest: manifest(UpdateComponent::Desktop, "0.4.0-rc.2", true),
            github_prerelease: true,
            published_at: None,
        }];
        let picked = select_compatible(&candidates, &ctx(Channel::Preview, "0.2.9")).unwrap();
        assert_eq!(picked.version, "0.4.0-rc.2");
    }

    #[test]
    fn switching_to_stable_does_not_downgrade() {
        let candidates = [ReleaseCandidate {
            manifest: manifest(UpdateComponent::Desktop, "0.3.0", false),
            github_prerelease: false,
            published_at: None,
        }];
        // Current is a newer preview; stable 0.3.0 is not newer.
        assert!(select_compatible(&candidates, &ctx(Channel::Stable, "0.4.0-rc.1")).is_none());
    }

    #[test]
    fn platform_mismatch_is_skipped() {
        let mut m = manifest(UpdateComponent::Desktop, "1.0.0", false);
        m.assets[0].platform = "macos".into();
        let candidates = [ReleaseCandidate {
            manifest: m,
            github_prerelease: false,
            published_at: None,
        }];
        assert!(select_compatible(&candidates, &ctx(Channel::Stable, "0.2.9")).is_none());
    }

    #[test]
    fn irreversible_migration_is_not_auto_selected() {
        let mut m = manifest(UpdateComponent::Desktop, "9.0.0", false);
        m.data_format.irreversible_migration = true;
        let candidates = [ReleaseCandidate {
            manifest: m,
            github_prerelease: false,
            published_at: None,
        }];
        assert!(select_compatible(&candidates, &ctx(Channel::Stable, "0.2.9")).is_none());
    }
}
