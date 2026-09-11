# Remote Manager end-to-end loop against the disposable dev host.
#
# fast : rebuild the Agent and Broker only, hand them to bootstrap through the
#        VELLUM_REMOTE_*_AMD64 seam, and converge a pristine host. No manifest,
#        no pinned Codex, no proxy image, no installer.
# full : stage a complete dev payload first, rebuild the Desktop so build.rs
#        re-embeds the manifest hash, then also install the pinned Codex the
#        way a real deployment does.
[CmdletBinding()]
param(
    [ValidateSet("fast", "full")]
    [string] $Lane = "fast",
    [switch] $KeepHost,
    [switch] $SkipComponentBuild
)

$ErrorActionPreference = "Stop"
$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
$OutputEncoding = $utf8

$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$componentRoot = Join-Path $repo "target\dev-host\linux-amd64"
$started = Get-Date

function Invoke-Checked([string] $Name, [scriptblock] $Work) {
    Write-Host ""
    Write-Host "==> $Name"
    & $Work
    if ($LASTEXITCODE -ne 0) { throw "$Name failed with exit code $LASTEXITCODE" }
}

function Elapsed { "{0:mm\:ss}" -f ([TimeSpan]::FromSeconds(((Get-Date) - $started).TotalSeconds)) }

if ($Lane -eq "full") {
    Invoke-Checked "Stage complete dev payload (amd64 + arm64)" {
        # The manifest is a two-architecture contract. A clean QA checkout has
        # nothing to "carry" for arm64, so stage both architectures here rather
        # than depending on artifacts left by an earlier developer run.
        & (Join-Path $PSScriptRoot "stage-remote-dev.ps1") -Arch @("amd64", "arm64")
    }
    # The manifest hash is compiled in, so the Desktop must be rebuilt before
    # it will accept the payload that was just staged.
    Invoke-Checked "Rebuild Desktop against the new manifest" {
        cargo build --manifest-path (Join-Path $repo "src-tauri\Cargo.toml") -p vellum --lib
    }
} elseif (-not $SkipComponentBuild) {
    Invoke-Checked "Build Linux amd64 Agent and Broker" {
        docker buildx build `
            --platform linux/amd64 `
            --file (Join-Path $repo "deploy\remote-bundle\Dockerfile") `
            --target export `
            --output "type=local,dest=$componentRoot" `
            $repo
    }
}

# The fast lane installs what was just built rather than what is staged. The
# full lane deliberately leaves these unset so the staged payload is exercised.
if ($Lane -eq "fast") {
    $agent = Join-Path $componentRoot "vellum-remote-agent"
    $broker = Join-Path $componentRoot "vellum-remote-broker"
    foreach ($artifact in @($agent, $broker)) {
        if (-not (Test-Path -LiteralPath $artifact -PathType Leaf)) {
            throw "missing $artifact; drop -SkipComponentBuild"
        }
    }
    $env:VELLUM_REMOTE_AGENT_AMD64 = $agent
    $env:VELLUM_REMOTE_BROKER_AMD64 = $broker
} else {
    Remove-Item Env:\VELLUM_REMOTE_AGENT_AMD64 -ErrorAction SilentlyContinue
    Remove-Item Env:\VELLUM_REMOTE_BROKER_AMD64 -ErrorAction SilentlyContinue
}

Invoke-Checked "Boot a pristine dev host" {
    pwsh -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "dev-host.ps1") reset
}

# --test-threads=1 because the cases install onto one shared host, and each
# asserts on what the previous state of that host was.
$cargoArgs = @(
    "test",
    "--manifest-path", (Join-Path $repo "src-tauri\Cargo.toml"),
    "-p", "vellum",
    "--test", "remote_dev_host",
    "--", "--ignored", "--nocapture", "--test-threads=1"
)
if ($Lane -ne "full") {
    # The pinned Codex case pushes ~250 MB over SSH; the fast lane skips it.
    $cargoArgs += "bootstrap_"
}
Invoke-Checked "Run remote_dev_host ($Lane)" { cargo @cargoArgs }

if (-not $KeepHost) {
    Write-Host ""
    Write-Host "==> Removing the dev host (pass -KeepHost to inspect it instead)"
    pwsh -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "dev-host.ps1") down | Out-Null
} else {
    Write-Host ""
    Write-Host "dev host kept: ssh vellum-dev-host"
}

Write-Host ""
Write-Host "remote e2e ($Lane) passed in $(Elapsed)"
