# Stage a development remote payload.
#
# The release path (build-local-release.ps1) rebuilds both architectures, native
# proxy daemons, and re-downloads ~480 MB of pinned Codex on every run, because
# a release must be buildable from nothing. A development iteration changes the
# Agent, proxy daemon, or Desktop -- never the pinned Codex, sometimes the proxy
# daemon, and usually only the architecture of the host being tested against.
# This script stages exactly what changed and reuses the rest from a
# content-addressed cache under target/remote-dev-cache.
#
# The payload it writes is deliberately NOT a release: releaseVersion carries a
# `+dev.<fingerprint>` suffix, so Assert-VellumStagedRemotePayload -- which the
# installer build runs -- rejects it, and Remote Manager displays the suffix.
[CmdletBinding()]
param(
    [ValidateSet("amd64", "arm64")]
    [string[]] $Arch = @("amd64"),
    [string] $CodexVersion = "0.147.0-alpha.6.6"
)

$ErrorActionPreference = "Stop"
$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
$OutputEncoding = $utf8

$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
. (Join-Path $PSScriptRoot "build-provenance.ps1")
$resourceRoot = Get-VellumRemoteResourceRoot $repo
$cacheRoot = Join-Path $repo "target\remote-dev-cache"
$packageJson = [IO.File]::ReadAllText((Join-Path $repo "package.json"), [Text.Encoding]::UTF8)
$version = ($packageJson | ConvertFrom-Json).version
$fingerprint = Get-VellumProxySourceFingerprint $repo
$allArch = @("amd64", "arm64")

function Invoke-Checked([string] $Name, [scriptblock] $Work) {
    Write-Host "==> $Name"
    & $Work
    if ($LASTEXITCODE -ne 0) { throw "$Name failed with exit code $LASTEXITCODE" }
}

function Sha256([string] $Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Copy-Into([string] $Source, [string] $Destination) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Destination) | Out-Null
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
}

function Resolve-Tar {
    # Never `tar.exe` off PATH: with Git for Windows ahead of System32 that is
    # GNU tar, which reads `C:\...` as a `host:path` remote spec and fails with
    # "Cannot connect to C: resolve failed". Windows ships bsdtar, which does not.
    $bundled = Join-Path $env:SystemRoot "System32\tar.exe"
    if (Test-Path -LiteralPath $bundled -PathType Leaf) { return $bundled }
    return "tar.exe"
}

function Get-CachedCodex([string] $Architecture) {
    $target = if ($Architecture -eq "arm64") { "aarch64-unknown-linux-musl" } else { "x86_64-unknown-linux-musl" }
    $cached = Join-Path $cacheRoot "codex\$CodexVersion\$target\codex"
    if (Test-Path -LiteralPath $cached -PathType Leaf) {
        Write-Host "==> Reuse cached Codex $CodexVersion ($Architecture)"
        return $cached
    }
    # Windows bsdtar cannot reliably open paths containing non-ASCII text, and
    # this repository lives under one. Stage under the ASCII TEMP path.
    $scratch = Join-Path $env:TEMP "vellum-codex-dev-$PID-$Architecture"
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $scratch | Out-Null
    try {
        $archive = Join-Path $scratch "codex.tar.gz"
        $url = "https://github.com/openai/codex/releases/download/rust-v$CodexVersion/codex-$target.tar.gz"
        Invoke-Checked "Download pinned Codex $CodexVersion ($Architecture)" {
            curl.exe -fsSL --proto '=https' --tlsv1.2 $url -o $archive
        }
        $tar = Resolve-Tar
        Invoke-Checked "Extract pinned Codex ($Architecture)" {
            & $tar -xzf $archive -C $scratch "codex-$target"
        }
        Copy-Into (Join-Path $scratch "codex-$target") $cached
    } finally {
        Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
    }
    return $cached
}

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw "Docker Desktop with buildx is required"
}

foreach ($architecture in $Arch) {
    $destination = Join-Path $resourceRoot "linux-$architecture"
    New-Item -ItemType Directory -Force -Path $destination | Out-Null
    $scratch = Join-Path $cacheRoot "components\$architecture"
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $scratch | Out-Null

    Invoke-Checked "Build Linux $architecture musl Agent and native Proxy" {
        docker buildx build `
            --build-arg "REMOTE_ARCH=$architecture" `
            --file (Join-Path $repo "deploy\remote-bundle\Dockerfile") `
            --target export `
            --output "type=local,dest=$scratch" `
            $repo
    }
    Copy-Into (Join-Path $scratch "vellum-remote-agent") (Join-Path $destination "vellum-remote-agent")
    Copy-Into (Join-Path $scratch "vellum-proxy-daemon") (Join-Path $destination "vellum-proxy-daemon")
    Copy-Into (Get-CachedCodex $architecture) (Join-Path $destination "codex")

}

# The manifest schema requires both architectures. One that was not rebuilt
# keeps whatever is already staged and is re-hashed honestly, so the manifest
# never claims bytes that are not on disk.
$carried = @()
foreach ($architecture in $allArch) {
    if ($Arch -contains $architecture) { continue }
    foreach ($name in @("vellum-remote-agent", "vellum-proxy-daemon", "codex")) {
        $artifact = Join-Path $resourceRoot "linux-$architecture\$name"
        if (-not (Test-Path -LiteralPath $artifact -PathType Leaf)) {
            throw "linux-$architecture/$name is not staged. Stage it once with -Arch $architecture, or run scripts/build-local-release.ps1 -Mode stage-only."
        }
    }
    $carried += $architecture
}

$agentVersion = (Select-String -Path (Join-Path $repo "crates\vellum-remote-agent\Cargo.toml") -Pattern '^version = "(.+)"$').Matches[0].Groups[1].Value
$manifestPath = Join-Path $resourceRoot "manifest.json"
Invoke-Checked "Finalize schema 5 native development manifest" {
    node (Join-Path $repo "scripts\finalize-remote-linux-manifest.mjs") $resourceRoot "$version+dev.$fingerprint" $CodexVersion $agentVersion $fingerprint
}

Write-Host ""
Write-Host "Dev payload staged: $version+dev.$fingerprint"
Write-Host "  rebuilt : $($Arch -join ', ')"
if ($carried.Count -gt 0) {
    Write-Host "  carried : $($carried -join ', ') (kept from the previous staging, re-hashed)"
}
Write-Host "  proxy   : native musl daemon"
Write-Host "  manifest: $(Sha256 $manifestPath)"
Write-Host ""
Write-Host "Rebuild the Desktop so build.rs re-embeds this manifest hash:"
Write-Host "  cargo build -p vellum          # or: pnpm tauri dev"
