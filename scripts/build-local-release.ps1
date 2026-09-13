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

function BundleArtifact([string] $Relative, [string] $Path) {
    [ordered]@{
        url = "bundle://$Relative"
        sha256 = Sha256 $Path
    }
}

function Test-CompleteArch([string] $ArchRoot, [string] $ExpectedImage) {
    foreach ($name in @("vellum-remote-agent", "vellum-remote-broker", "codex", "proxy-image.tar")) {
        $artifact = Join-Path $ArchRoot $name
        if (-not (Test-Path -LiteralPath $artifact -PathType Leaf) -or
            (Get-Item -LiteralPath $artifact).Length -eq 0) {
            return $false
        }
    }
    $imageMarker = Join-Path $ArchRoot "proxy-image.ref"
    if (-not (Test-Path -LiteralPath $imageMarker -PathType Leaf) -or
        [IO.File]::ReadAllText($imageMarker, [Text.Encoding]::UTF8).Trim() -ne $ExpectedImage) {
        return $false
    }
    return $true
}

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
        Assert-VellumResumeFingerprint -ExpectedFingerprint $proxyFingerprint -ArchRoot $archRoot -ExpectedImage $proxyImage
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

    Invoke-Checked "Build Linux $arch Agent and Broker" {
        docker buildx build `
            --platform "linux/$arch" `
            --file (Join-Path $repo "deploy\remote-bundle\Dockerfile") `
            --target export `
            --output "type=local,dest=$archRoot" `
            $repo
    }

    $codexTarget = if ($arch -eq "arm64") { "aarch64-unknown-linux-musl" } else { "x86_64-unknown-linux-musl" }
    # Windows bsdtar cannot reliably open paths containing non-ASCII text.
    # Download/extract under the ASCII TEMP path, then copy the binary back.
    $codexTemp = Join-Path $env:TEMP "vellum-codex-$PID-$arch"
    if (Test-Path -LiteralPath $codexTemp) {
        Remove-Item -LiteralPath $codexTemp -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $codexTemp | Out-Null
    $codexArchive = Join-Path $codexTemp "codex.tar.gz"
    $codexUrl = "https://github.com/openai/codex/releases/download/rust-v$CodexVersion/codex-$codexTarget.tar.gz"
    Invoke-Checked "Download pinned Codex $CodexVersion for $arch" {
        curl.exe -fsSL --proto '=https' --tlsv1.2 $codexUrl -o $codexArchive
    }
    Invoke-Checked "Extract pinned Codex for $arch" {
        tar.exe -xzf $codexArchive -C $codexTemp "codex-$codexTarget"
    }
    Copy-Item -LiteralPath (Join-Path $codexTemp "codex-$codexTarget") -Destination (Join-Path $archRoot "codex") -Force
    Remove-Item -LiteralPath $codexTemp -Recurse -Force

    $proxyArchive = Join-Path $archRoot "proxy-image.tar"
    Invoke-Checked "Build Proxy OCI archive for $arch" {
        docker buildx build `
            --platform "linux/$arch" `
            --file (Join-Path $repo "deploy\proxy\Dockerfile") `
            --tag $proxyImage `
            --output "type=docker,dest=$proxyArchive" `
            $repo
    }
    [IO.File]::WriteAllText((Join-Path $archRoot "proxy-image.ref"), "$proxyImage`n", $utf8)
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
    foreach ($name in @("vellum-remote-agent", "vellum-remote-broker", "codex", "proxy-image.tar")) {
        Copy-Item -LiteralPath (Join-Path $staged[$arch] $name) -Destination (Join-Path $destination $name) -Force
    }
}

$amd64 = Join-Path $resourceRoot "linux-amd64"
$arm64 = Join-Path $resourceRoot "linux-arm64"
$agentVersion = (Select-String -Path (Join-Path $repo "crates\vellum-remote-agent\Cargo.toml") -Pattern '^version = "(.+)"$').Matches[0].Groups[1].Value
$brokerVersion = (Select-String -Path (Join-Path $repo "crates\vellum-remote-broker\Cargo.toml") -Pattern '^version = "(.+)"$').Matches[0].Groups[1].Value
$manifest = [ordered]@{
    schemaVersion = 3
    releaseVersion = $version
    codex = [ordered]@{
        pinnedVersion = $CodexVersion
        compatibleRange = $CodexVersion
        artifacts = [ordered]@{
            'linux-x64' = BundleArtifact "linux-amd64/codex" (Join-Path $amd64 "codex")
            'linux-arm64' = BundleArtifact "linux-arm64/codex" (Join-Path $arm64 "codex")
        }
    }
    agent = [ordered]@{
        version = $agentVersion
        artifacts = [ordered]@{
            'linux-x64' = BundleArtifact "linux-amd64/vellum-remote-agent" (Join-Path $amd64 "vellum-remote-agent")
            'linux-arm64' = BundleArtifact "linux-arm64/vellum-remote-agent" (Join-Path $arm64 "vellum-remote-agent")
        }
    }
    broker = [ordered]@{
        version = $brokerVersion
        artifacts = [ordered]@{
            'linux-x64' = BundleArtifact "linux-amd64/vellum-remote-broker" (Join-Path $amd64 "vellum-remote-broker")
            'linux-arm64' = BundleArtifact "linux-arm64/vellum-remote-broker" (Join-Path $arm64 "vellum-remote-broker")
        }
    }
    proxy = [ordered]@{
        image = $proxyImage
        artifacts = [ordered]@{
            'linux-x64' = BundleArtifact "linux-amd64/proxy-image.tar" (Join-Path $amd64 "proxy-image.tar")
            'linux-arm64' = BundleArtifact "linux-arm64/proxy-image.tar" (Join-Path $arm64 "proxy-image.tar")
        }
    }
    protocolVersion = 3
}
$manifestPath = Join-Path $resourceRoot "manifest.json"
$manifestJson = $manifest | ConvertTo-Json -Depth 8
[IO.File]::WriteAllText($manifestPath, $manifestJson, (New-Object Text.UTF8Encoding($false)))

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
