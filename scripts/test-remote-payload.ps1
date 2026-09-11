# Negative tests for Remote Manager payload staging. These do not need Docker.
$ErrorActionPreference = "Stop"
$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
$OutputEncoding = $utf8

$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
. (Join-Path $PSScriptRoot "build-provenance.ps1")

function New-FakeArtifact([string] $Path, [string] $Content) {
    $parent = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    [IO.File]::WriteAllText($Path, $Content, $utf8)
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function New-FakePayload {
    param(
        [string] $Root,
        [string] $Version = "0.2.3",
        [string] $ProxyImage = "vellum-proxy:0.2.3-testhash12ab",
        [switch] $SkipManifest,
        [switch] $SkipAmd64Agent,
        [string] $ManifestVersion,
        [string] $ManifestImage,
        [switch] $MismatchHash
    )
    if (Test-Path -LiteralPath $Root) {
        Remove-Item -LiteralPath $Root -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $Root | Out-Null
    $hashes = @{}
    foreach ($arch in @("linux-amd64", "linux-arm64")) {
        foreach ($name in @("vellum-remote-agent", "vellum-remote-broker", "codex", "proxy-image.tar")) {
            if ($SkipAmd64Agent -and $arch -eq "linux-amd64" -and $name -eq "vellum-remote-agent") {
                continue
            }
            $relative = "$arch/$name"
            $hashes[$relative] = New-FakeArtifact (Join-Path $Root ($relative -replace '/', '\')) "$arch $name"
        }
    }
    if ($SkipManifest) {
        return
    }
    $agentAmd = $hashes["linux-amd64/vellum-remote-agent"]
    if ($MismatchHash -and $agentAmd) {
        $agentAmd = "0" * 64
    }
    $manifest = [ordered]@{
        schemaVersion = 3
        releaseVersion = $(if ($ManifestVersion) { $ManifestVersion } else { $Version })
        codex = [ordered]@{
            pinnedVersion = "0.147.0-alpha.6.6"
            compatibleRange = "0.147.0-alpha.6.6"
            artifacts = [ordered]@{
                "linux-x64" = [ordered]@{ url = "bundle://linux-amd64/codex"; sha256 = $hashes["linux-amd64/codex"] }
                "linux-arm64" = [ordered]@{ url = "bundle://linux-arm64/codex"; sha256 = $hashes["linux-arm64/codex"] }
            }
        }
        agent = [ordered]@{
            version = "0.3.0"
            artifacts = [ordered]@{
                "linux-x64" = [ordered]@{ url = "bundle://linux-amd64/vellum-remote-agent"; sha256 = $agentAmd }
                "linux-arm64" = [ordered]@{ url = "bundle://linux-arm64/vellum-remote-agent"; sha256 = $hashes["linux-arm64/vellum-remote-agent"] }
            }
        }
        broker = [ordered]@{
            version = "0.1.0"
            artifacts = [ordered]@{
                "linux-x64" = [ordered]@{ url = "bundle://linux-amd64/vellum-remote-broker"; sha256 = $hashes["linux-amd64/vellum-remote-broker"] }
                "linux-arm64" = [ordered]@{ url = "bundle://linux-arm64/vellum-remote-broker"; sha256 = $hashes["linux-arm64/vellum-remote-broker"] }
            }
        }
        proxy = [ordered]@{
            image = $(if ($ManifestImage) { $ManifestImage } else { $ProxyImage })
            artifacts = [ordered]@{
                "linux-x64" = [ordered]@{ url = "bundle://linux-amd64/proxy-image.tar"; sha256 = $hashes["linux-amd64/proxy-image.tar"] }
                "linux-arm64" = [ordered]@{ url = "bundle://linux-arm64/proxy-image.tar"; sha256 = $hashes["linux-arm64/proxy-image.tar"] }
            }
        }
        protocolVersion = 3
    }
    [IO.File]::WriteAllText((Join-Path $Root "manifest.json"), ($manifest | ConvertTo-Json -Depth 8), $utf8)
}

function Assert-Throws([string] $Name, [scriptblock] $Work, [string] $Needle) {
    $caught = $null
    try {
        & $Work
    } catch {
        $caught = $_.Exception.Message
    }
    if (-not $caught) {
        throw "expected $Name to fail"
    }
    if ($caught -notmatch [regex]::Escape($Needle) -and $caught -notlike "*$Needle*") {
        throw "$Name failed with unexpected message: $caught"
    }
    Write-Host "PASS $Name"
}

$temp = Join-Path $env:TEMP "vellum-remote-payload-tests-$PID"
New-Item -ItemType Directory -Force -Path $temp | Out-Null
try {
    $version = "0.2.3"
    $image = "vellum-proxy:0.2.3-testhash12ab"

    $missing = Join-Path $temp "missing-manifest"
    New-FakePayload -Root $missing -SkipManifest
    Assert-Throws "missing manifest" {
        Assert-VellumStagedRemotePayload -ResourceRoot $missing -ExpectedVersion $version -ExpectedProxyImage $image
    } "manifest.json"

    $stale = Join-Path $temp "stale-version"
    New-FakePayload -Root $stale -ManifestVersion "0.2.2"
    Assert-Throws "stale releaseVersion" {
        Assert-VellumStagedRemotePayload -ResourceRoot $stale -ExpectedVersion $version -ExpectedProxyImage $image
    } "0.2.2"

    # scripts/stage-remote-dev.ps1 marks its output with a `+dev.<fingerprint>`
    # suffix. This is the check that keeps such a payload out of an installer.
    $devPayload = Join-Path $temp "dev-payload"
    New-FakePayload -Root $devPayload -ManifestVersion "0.2.3+dev.deadbeef1234"
    Assert-Throws "dev payload rejected" {
        Assert-VellumStagedRemotePayload -ResourceRoot $devPayload -ExpectedVersion $version -ExpectedProxyImage $image
    } "+dev.deadbeef1234"

    $oldImage = Join-Path $temp "old-image"
    New-FakePayload -Root $oldImage -ManifestImage "vellum-proxy:0.2.2-5a852db4c27a"
    Assert-Throws "stale proxy image" {
        Assert-VellumStagedRemotePayload -ResourceRoot $oldImage -ExpectedVersion $version -ExpectedProxyImage $image
    } "0.2.2"

    $missingArch = Join-Path $temp "missing-arch"
    New-FakePayload -Root $missingArch -SkipAmd64Agent
    Assert-Throws "missing amd64 agent" {
        Assert-VellumStagedRemotePayload -ResourceRoot $missingArch -ExpectedVersion $version -ExpectedProxyImage $image
    } "linux-amd64/vellum-remote-agent"

    $mismatch = Join-Path $temp "hash-mismatch"
    New-FakePayload -Root $mismatch -MismatchHash
    Assert-Throws "hash mismatch" {
        Assert-VellumStagedRemotePayload -ResourceRoot $mismatch -ExpectedVersion $version -ExpectedProxyImage $image
    } "does not match manifest"

    $ok = Join-Path $temp "ok"
    New-FakePayload -Root $ok
    $staged = Assert-VellumStagedRemotePayload -ResourceRoot $ok -ExpectedVersion $version -ExpectedProxyImage $image
    if ($staged.ReleaseVersion -ne $version) { throw "valid payload returned wrong version" }
    if (-not $staged.ManifestSha256) { throw "valid payload missing manifest hash" }
    Write-Host "PASS valid payload"

    $embedded = Join-Path $temp "embedded-mismatch"
    New-FakePayload -Root $embedded
    [IO.File]::WriteAllText((Join-Path $embedded "linux-amd64\codex"), "mutated", $utf8)
    Assert-Throws "embedded artifact drift" {
        Assert-VellumEmbeddedRemotePayload -Staged $staged -EmbeddedRoot $embedded
    } "does not match"

    $buildMain = [IO.File]::ReadAllText((Join-Path $repo "scripts\build-main.ps1"), $utf8)
    if ($buildMain -notmatch 'build-local-release\.ps1') {
        throw "build-main.ps1 must invoke build-local-release.ps1"
    }
    if ($buildMain -notmatch '-Mode stage-only') {
        throw "build-main.ps1 must stage remote payload with -Mode stage-only"
    }
    if ($buildMain -match 'build-local-release\.ps1[^\r\n]*Resume') {
        throw "build:main must not pass -Resume to remote staging"
    }
    $stageAt = $buildMain.IndexOf('-Mode stage-only')
    $buildAt = $buildMain.IndexOf('pnpm --dir $source.SourceWorktree run build')
    if ($stageAt -lt 0 -or $buildAt -lt 0 -or $stageAt -gt $buildAt) {
        throw "build:main must stage the remote payload before pnpm run build"
    }
    Write-Host "PASS build-main stages remote payload before Tauri"

    $localRelease = [IO.File]::ReadAllText((Join-Path $repo "scripts\build-local-release.ps1"), $utf8)
    if ($localRelease -notmatch 'stage-only refuses -Resume') {
        throw "stage-only must refuse Resume"
    }
    Write-Host "PASS stage-only refuses Resume"
}
finally {
    Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "All remote payload tests passed."
