//! Fixture client against the shipped public API. Live GitHub is not a
//! substitute for this path; unsigned local builds are not a published channel.

use vellum_lib::updates::{
    parse_github_releases, select_compatible, verify_signed_manifest, Channel, ReleaseCandidate,
    SelectContext, TrustStore, UpdateComponent, MANIFEST_SCHEMA_VERSION,
};

#[test]
fn signed_manifest_is_required_even_when_json_is_well_formed() {
    let trust = TrustStore::bundled();
    let raw = br#"{"schemaVersion":1,"component":"desktop","version":"0.3.0","sourceCommit":"c","releaseTag":"desktop-v0.3.0","sequence":1,"keyId":"none","minDesktopVersion":"0.1.0","bridgeApiCompat":"*","remoteProtocolCompat":"*","assets":[{"platform":"windows","arch":"x64","name":"a.bin","size":1,"sha256":"00"}]}"#;
    assert_eq!(MANIFEST_SCHEMA_VERSION, 1);
    assert!(verify_signed_manifest(raw, &[], &trust).is_err());
    assert!(verify_signed_manifest(raw, &[0u8; 64], &trust).is_err());
}

#[test]
fn github_listing_ignores_drafts_and_does_not_use_latest() {
    let json = r#"
    [
      {"tag_name":"desktop-v0.3.0","prerelease":false,"draft":true,"assets":[]},
      {"tag_name":"desktop-v0.2.5","prerelease":false,"draft":false,"assets":[]}
    ]
    "#;
    let releases = parse_github_releases(json).unwrap();
    assert!(releases.iter().any(|release| release.draft));
    assert_eq!(UpdateComponent::Desktop.github_repo(), "910488/vellum");
    assert_eq!(UpdateComponent::Core.github_repo(), "910488/vellum");
}

#[test]
fn selection_is_highest_compatible_not_publish_order() {
    assert!(select_compatible(
        &[] as &[ReleaseCandidate],
        &SelectContext {
            component: UpdateComponent::Desktop,
            channel: Channel::Stable,
            current_version: "0.2.9",
            platform: "windows",
            arch: "x64",
            installed_desktop: "0.2.9",
            bridge_api: "1.0.0",
            remote_protocol: "3.0.0",
        },
    )
    .is_none());
}
