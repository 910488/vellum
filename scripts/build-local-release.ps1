[CmdletBinding()]
param(
    [string] $CodexVersion = "0.147.0-alpha.6.6",
    [ValidateSet("stage-only", "nsis")]
    [string] $Mode = "nsis",
    [switch] $Resume
)

$ErrorActionPreference = "Stop"
$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
$OutputEncoding = $utf8

$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
. (Join-Path $PSScriptRoot "build-provenance.ps1")
$resourceRoot = Join-Path $repo "src-tauri\resources\remote"
$scratchRoot = Join-Path $repo "target\local-release"
$packageJson = [IO.File]::ReadAllText((Join-Path $repo "package.json"), [Text.Encoding]::UTF8)
$version = ($packageJson | ConvertFrom-Json).version

function Invoke-Checked([string] $Name, [scriptblock] $Work) {
    Write-Host "==> $Name"
    & $Work
    if ($LASTEXITCODE -ne 0) {
        throw "$Name failed with exit code $LASTEXITCODE"
    }
}

function Sha256([string] $Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

if ($Resume -and $Mode -eq "stage-only") {
    throw "stage-only refuses -Resume; both Linux architectures must be rebuilt from the current source fingerprint"
}

$proxyFingerprint = Get-VellumProxySourceFingerprint $repo
$proxyImage = Get-VellumExpectedProxyImage $repo $version

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw "Docker Desktop with buildx is required"
}
if ($Mode -ne "stage-only" -and -not (Get-Command pnpm -ErrorAction SilentlyContinue)) {
    throw "pnpm is required"
}

New-Item -ItemType Directory -Force -Path $scratchRoot | Out-Null
$staged = @{}
foreach ($arch in @("amd64", "arm64")) {
    $archRoot = Join-Path $scratchRoot $arch
    if ($Resume) {
        Assert-VellumResumeFingerprint -ExpectedFingerprint $proxyFingerprint -ArchRoot $archRoot -ExpectedImage "${proxyImage}:$CodexVersion"
        Write-Host "==> Reuse completed Linux $arch payload"
        $staged[$arch] = $archRoot
        continue
    }
    if (Test-Path -LiteralPath $archRoot) {
        $resolvedScratch = (Resolve-Path $scratchRoot).Path
        $resolvedArch = (Resolve-Path $archRoot).Path
        if (-not $resolvedArch.StartsWith($resolvedScratch + [IO.Path]::DirectorySeparatorChar)) {
            throw "refusing to clear unexpected path $resolvedArch"
        }
        Remove-Item -LiteralPath $resolvedArch -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $archRoot | Out-Null

    Invoke-Checked "Build Linux $arch musl Agent and native Proxy" {
        docker buildx build `
            --build-arg "REMOTE_ARCH=$arch" `
            --file (Join-Path $repo "deploy\remote-bundle\Dockerfile") `
            --target export `
            --output "type=local,dest=$archRoot" `
            $repo
    }

    $codexTarget = if ($arch -eq "arm64") { "aarch64-unknown-linux-musl" } else { "x86_64-unknown-linux-musl" }
    Invoke-Checked "Download digest-verified Codex runtime package $CodexVersion for $arch" {
        node (Join-Path $repo "scripts\download-codex-package.mjs") $CodexVersion $codexTarget (Join-Path $archRoot "codex-package.tar.gz")
    }

    [IO.File]::WriteAllText((Join-Path $archRoot "native-source.ref"), "${proxyImage}:$CodexVersion`n", $utf8)
    $staged[$arch] = $archRoot
}

foreach ($arch in @("amd64", "arm64")) {
    $destination = Join-Path $resourceRoot "linux-$arch"
    if (Test-Path -LiteralPath $destination) {
        $resolvedResource = (Resolve-Path $resourceRoot).Path
        $resolvedDestination = (Resolve-Path $destination).Path
        if (-not $resolvedDestination.StartsWith($resolvedResource + [IO.Path]::DirectorySeparatorChar)) {
            throw "refusing to replace unexpected resource path $resolvedDestination"
        }
        Remove-Item -LiteralPath $resolvedDestination -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $destination | Out-Null
    foreach ($name in @("vellum-remote-agent", "vellum-proxy-daemon", "codex-package.tar.gz")) {
        Copy-Item -LiteralPath (Join-Path $staged[$arch] $name) -Destination (Join-Path $destination $name) -Force
    }
}

$agentVersion = (Select-String -Path (Join-Path $repo "crates\vellum-remote-agent\Cargo.toml") -Pattern '^version = "(.+)"$').Matches[0].Groups[1].Value
Invoke-Checked "Finalize schema 5 native remote manifest" {
    node (Join-Path $repo "scripts\finalize-remote-linux-manifest.mjs") $resourceRoot $version $CodexVersion $agentVersion $proxyFingerprint
}

$staged = Assert-VellumStagedRemotePayload `
    -ResourceRoot $resourceRoot `
    -ExpectedVersion $version `
    -ExpectedProxyImage $proxyImage
Write-Host "Staged remote payload $($staged.ReleaseVersion) image $($staged.ProxyImage)"
Write-Host "Manifest SHA-256: $($staged.ManifestSha256)"

if ($Mode -eq "stage-only") {
    Write-Host "stage-only complete; installer was not built"
    return
}

Invoke-Checked "Install JavaScript dependencies" { pnpm --dir $repo install --frozen-lockfile }
Invoke-Checked "Build Vellum $version NSIS with embedded remote bundle" {
    pnpm --dir $repo exec tauri build --bundles nsis
}

$installerPath = Join-Path $repo "target\release\bundle\nsis\Vellum_${version}_x64-setup.exe"
if (-not (Test-Path -LiteralPath $installerPath -PathType Leaf)) {
    throw "NSIS installer was not produced"
}
Write-Host ""
Write-Host "Local release ready: $installerPath"
Write-Host "SHA-256: $(Sha256 $installerPath)"
