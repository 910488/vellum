# Shared build provenance helpers. build-main.ps1 is the shipped entry
# point; this file holds the pure git/file checks so they can be reused
# by build-local-release.ps1 without duplicating the rules.

function Get-VellumFileSha256([string] $Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-VellumProxySourceFingerprint([string] $Repo) {
    $tracked = @(& git -C $Repo ls-files -- Cargo.toml Cargo.lock crates src-tauri/Cargo.toml src-tauri/src third_party/codex-0.150 deploy/proxy/Dockerfile deploy/remote-bundle/Dockerfile | Sort-Object)
    if ($LASTEXITCODE -ne 0 -or $tracked.Count -eq 0) {
        throw "unable to enumerate proxy image source files"
    }
    $material = ($tracked | ForEach-Object {
        $path = Join-Path $Repo $_
        "$_`0$(Get-VellumFileSha256 $path)`n"
    }) -join ""
    $hasher = [Security.Cryptography.SHA256]::Create()
    try {
        $digest = $hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes($material))
    } finally {
        $hasher.Dispose()
    }
    (([BitConverter]::ToString($digest) -replace '-', '').ToLowerInvariant()).Substring(0, 12)
}

function Get-VellumWorktreeStatus([string] $Worktree) {
    $head = (& git -C $Worktree rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $head) {
        throw "unable to resolve HEAD in $Worktree"
    }
    $branch = (& git -C $Worktree rev-parse --abbrev-ref HEAD).Trim()
    $dirty = @(& git -C $Worktree status --short)
    [pscustomobject]@{
        Worktree = $Worktree
        Head = $head
        Branch = $branch
        Dirty = @($dirty)
        Clean = ($dirty.Count -eq 0)
    }
}

function Resolve-VellumBuildSource {
    param(
        [Parameter(Mandatory = $true)]
        [string] $InvokingWorktree,
        [string] $SourceWorktree,
        [string] $ExpectedRef = "origin/main",
        [switch] $AllowDevelopment,
        [switch] $FetchRemote
    )

    if ($FetchRemote) {
        & git -C $InvokingWorktree fetch --prune origin
        if ($LASTEXITCODE -ne 0) {
            throw "git fetch --prune origin failed with exit code $LASTEXITCODE"
        }
    }

    $expected = (& git -C $InvokingWorktree rev-parse $ExpectedRef).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $expected) {
        throw "unable to resolve expected source ref $ExpectedRef"
    }

    $resolvedWorktree = $SourceWorktree
    if (-not $resolvedWorktree) {
        $resolvedWorktree = (& git -C $InvokingWorktree for-each-ref --format="%(worktreepath)" refs/heads/main).Trim()
        if (-not $resolvedWorktree -or -not (Test-Path -LiteralPath $resolvedWorktree -PathType Container)) {
            throw "The main branch is not checked out in a worktree. Create a clean main worktree or pass -SourceWorktree."
        }
    } elseif (-not (Test-Path -LiteralPath $resolvedWorktree -PathType Container)) {
        throw "Source worktree does not exist: $resolvedWorktree"
    }

    $status = Get-VellumWorktreeStatus $resolvedWorktree
    $isMainBranch = $status.Branch -eq "main"
    $detachedAtExpected = ($status.Branch -eq "HEAD") -and ($status.Head -eq $expected)
    $matchesExpected = $status.Head -eq $expected

    if (-not $status.Clean) {
        throw "Source worktree is dirty; refusing to build. Commit or discard local changes first. Worktree: $($status.Worktree)"
    }

    $originMain = (& git -C $InvokingWorktree rev-parse origin/main).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $originMain) {
        throw "unable to resolve origin/main; fetch origin or pass a development source"
    }

    # Formal main releases are pinned to origin/main independently of the
    # caller-supplied ExpectedRef. A clean local `main` (or any other custom
    # ref) can only produce a development artifact.
    $formalMainSource = ($ExpectedRef -eq "origin/main") -and ($status.Head -eq $originMain)
    $onMainCheckout = $isMainBranch -or $detachedAtExpected

    $releaseKind = $null
    if ($formalMainSource -and $onMainCheckout -and -not $AllowDevelopment) {
        $releaseKind = "main"
    } elseif ($AllowDevelopment) {
        $releaseKind = "development"
    } elseif (-not $matchesExpected) {
        throw "Source worktree HEAD $($status.Head) does not match $ExpectedRef ($expected) on a clean main checkout. Pass -AllowDevelopment for a named repair worktree (artifact will not be labeled a main release)."
    } else {
        throw "Source ref '$ExpectedRef' is not origin/main (origin/main=$originMain). Custom refs can only produce development artifacts; pass -AllowDevelopment."
    }

    if ($releaseKind -eq "main" -and (-not $formalMainSource -or $status.Head -ne $originMain)) {
        throw "Main-release builds require HEAD to equal origin/main ($originMain); got ref $ExpectedRef commit $($status.Head)"
    }

    [pscustomobject]@{
        SourceRef = $ExpectedRef
        ExpectedCommit = $expected
        OriginMainCommit = $originMain
        SourceWorktree = $status.Worktree
        Commit = $status.Head
        Branch = $status.Branch
        Clean = $true
        ReleaseKind = $releaseKind
        MainRelease = ($releaseKind -eq "main")
        SourceFingerprint = (Get-VellumProxySourceFingerprint $status.Worktree)
    }
}

function Get-VellumArtifactLayout {
    param(
        [Parameter(Mandatory = $true)]
        [string] $PrimaryWorktree,
        [Parameter(Mandatory = $true)]
        [string] $ReleaseKind,
        [Parameter(Mandatory = $true)]
        [string] $Commit
    )

    if ($ReleaseKind -notin @("main", "development")) {
        throw "unknown release kind '$ReleaseKind'"
    }
    $kindRoot = Join-Path $PrimaryWorktree "artifacts\$ReleaseKind"
    $commitRoot = Join-Path $kindRoot $Commit
    [pscustomobject]@{
        KindRoot = $kindRoot
        CommitRoot = $commitRoot
        TargetDir = Join-Path $commitRoot "target"
        CommitManifest = Join-Path $commitRoot "build-info.json"
        PointerManifest = Join-Path $kindRoot "build-info.json"
    }
}

function Get-VellumExpectedProxyImage([string] $Repo, [string] $Version) {
    if ([string]::IsNullOrWhiteSpace($Version)) {
        throw "package version is required to name the proxy image"
    }
    $fingerprint = Get-VellumProxySourceFingerprint $Repo
    "vellum-proxy:${Version}-${fingerprint}"
}

function Get-VellumRemoteResourceRoot([string] $Repo) {
    Join-Path $Repo "src-tauri\resources\remote"
}

function Get-VellumRemoteArtifactLayout {
    [ordered]@{
        "linux-amd64/vellum-remote-agent" = "agent.linux-x64"
        "linux-amd64/vellum-remote-broker" = "broker.linux-x64"
        "linux-amd64/codex" = "codex.linux-x64"
        "linux-amd64/proxy-image.tar" = "proxy.linux-x64"
        "linux-arm64/vellum-remote-agent" = "agent.linux-arm64"
        "linux-arm64/vellum-remote-broker" = "broker.linux-arm64"
        "linux-arm64/codex" = "codex.linux-arm64"
        "linux-arm64/proxy-image.tar" = "proxy.linux-arm64"
    }
}

function Get-VellumManifestArtifactSha256($Manifest, [string] $Component, [string] $Arch) {
    $set = $Manifest.$Component
    if (-not $set) {
        throw "remote manifest is missing component '$Component'"
    }
    $artifact = $set.artifacts.$Arch
    if (-not $artifact) {
        throw "remote manifest is missing $Component artifact '$Arch'"
    }
    $hash = [string]$artifact.sha256
    if ($hash.Length -ne 64 -or ($hash -notmatch '^[0-9a-fA-F]{64}$')) {
        throw "remote manifest $Component/$Arch sha256 is not a 64-char hex digest"
    }
    $hash.ToLowerInvariant()
}

function Assert-VellumStagedRemotePayload {
    param(
        [Parameter(Mandatory = $true)]
        [string] $ResourceRoot,
        [Parameter(Mandatory = $true)]
        [string] $ExpectedVersion,
        [Parameter(Mandatory = $true)]
        [string] $ExpectedProxyImage
    )

    $manifestPath = Join-Path $ResourceRoot "manifest.json"
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
        throw "Remote payload missing manifest.json under $ResourceRoot. build:main cannot ship an installer without a freshly staged Remote Manager payload."
    }

    $manifest = Get-Content -LiteralPath $manifestPath -Raw -Encoding utf8 | ConvertFrom-Json
    if ([int]$manifest.schemaVersion -ne 3) {
        throw "Remote manifest schemaVersion must be 3; got '$($manifest.schemaVersion)'"
    }
    if ([string]$manifest.releaseVersion -ne $ExpectedVersion) {
        throw "Remote manifest releaseVersion '$($manifest.releaseVersion)' does not match package.json $ExpectedVersion"
    }
    if ([int]$manifest.protocolVersion -ne 3) {
        throw "Remote manifest protocolVersion must be 3; got '$($manifest.protocolVersion)'"
    }
    if ([string]$manifest.proxy.image -ne $ExpectedProxyImage) {
        throw "Remote proxy image '$($manifest.proxy.image)' does not match this source fingerprint image '$ExpectedProxyImage'. 0.2.2 payloads cannot be reused."
    }

    $hashes = [ordered]@{}
    foreach ($pair in (Get-VellumRemoteArtifactLayout).GetEnumerator()) {
        $relative = $pair.Key
        $path = Join-Path $ResourceRoot ($relative -replace '/', '\')
        if (-not (Test-Path -LiteralPath $path -PathType Leaf) -or (Get-Item -LiteralPath $path).Length -eq 0) {
            throw "Remote payload missing or empty artifact $relative"
        }
        $actual = Get-VellumFileSha256 $path
        $parts = $pair.Value.Split('.', 2)
        $expected = Get-VellumManifestArtifactSha256 $manifest $parts[0] $parts[1]
        if ($actual -ne $expected) {
            throw "Remote artifact $relative sha256 $actual does not match manifest $expected"
        }
        $hashes[$relative] = $actual
    }

    [pscustomobject]@{
        ManifestPath = $manifestPath
        ManifestSha256 = Get-VellumFileSha256 $manifestPath
        ReleaseVersion = [string]$manifest.releaseVersion
        ProtocolVersion = [int]$manifest.protocolVersion
        ProxyImage = [string]$manifest.proxy.image
        ArtifactSha256 = $hashes
    }
}

function Assert-VellumEmbeddedRemotePayload {
    param(
        [Parameter(Mandatory = $true)]
        $Staged,
        [Parameter(Mandatory = $true)]
        [string] $EmbeddedRoot
    )

    $embedded = Assert-VellumStagedRemotePayload `
        -ResourceRoot $EmbeddedRoot `
        -ExpectedVersion $Staged.ReleaseVersion `
        -ExpectedProxyImage $Staged.ProxyImage
    if ($embedded.ManifestSha256 -ne $Staged.ManifestSha256) {
        throw "Embedded remote manifest sha256 $($embedded.ManifestSha256) does not match staged $($Staged.ManifestSha256)"
    }
    foreach ($relative in $Staged.ArtifactSha256.Keys) {
        if ($embedded.ArtifactSha256[$relative] -ne $Staged.ArtifactSha256[$relative]) {
            throw "Embedded remote artifact $relative does not match the staged payload"
        }
    }
    $embedded
}

function New-VellumBuildInfo {
    param(
        [Parameter(Mandatory = $true)]
        $Source,
        [Parameter(Mandatory = $true)]
        [string] $Version,
        [string] $Executable,
        [string] $Installer,
        [string[]] $Bundles,
        [string] $Target,
        [string] $Renderer,
        [string] $BuiltAt,
        $Remote
    )

    if (-not $BuiltAt) {
        $BuiltAt = (Get-Date).ToUniversalTime().ToString("o")
    }

    $info = [ordered]@{
        sourceRef = $Source.SourceRef
        commit = $Source.Commit
        expectedCommit = $Source.ExpectedCommit
        originMainCommit = $Source.OriginMainCommit
        branch = $Source.Branch
        clean = [bool]$Source.Clean
        releaseKind = $Source.ReleaseKind
        mainRelease = [bool]$Source.MainRelease
        builtAt = $BuiltAt
        version = $Version
        sourceWorktree = $Source.SourceWorktree
        sourceFingerprint = $Source.SourceFingerprint
        renderer = $Renderer
        target = $Target
        executable = $Executable
        executableSha256 = if ($Executable) { Get-VellumFileSha256 $Executable } else { $null }
        installer = $Installer
        installerSha256 = if ($Installer) { Get-VellumFileSha256 $Installer } else { $null }
        bundles = @($Bundles)
    }
    if ($Remote) {
        $info.remoteManifestSha256 = $Remote.ManifestSha256
        $info.remoteReleaseVersion = $Remote.ReleaseVersion
        $info.remoteProtocolVersion = $Remote.ProtocolVersion
        $info.proxyImage = $Remote.ProxyImage
        $info.remoteArtifacts = [ordered]@{
            "linux-amd64" = [ordered]@{
                agent = $Remote.ArtifactSha256["linux-amd64/vellum-remote-agent"]
                broker = $Remote.ArtifactSha256["linux-amd64/vellum-remote-broker"]
                codex = $Remote.ArtifactSha256["linux-amd64/codex"]
                proxyArchive = $Remote.ArtifactSha256["linux-amd64/proxy-image.tar"]
            }
            "linux-arm64" = [ordered]@{
                agent = $Remote.ArtifactSha256["linux-arm64/vellum-remote-agent"]
                broker = $Remote.ArtifactSha256["linux-arm64/vellum-remote-broker"]
                codex = $Remote.ArtifactSha256["linux-arm64/codex"]
                proxyArchive = $Remote.ArtifactSha256["linux-arm64/proxy-image.tar"]
            }
        }
    }
    return $info
}

function Assert-VellumResumeFingerprint {
    param(
        [Parameter(Mandatory = $true)]
        [string] $ExpectedFingerprint,
        [Parameter(Mandatory = $true)]
        [string] $ArchRoot,
        [Parameter(Mandatory = $true)]
        [string] $ExpectedImage
    )

    foreach ($name in @("vellum-remote-agent", "vellum-remote-broker", "codex", "proxy-image.tar")) {
        $artifact = Join-Path $ArchRoot $name
        if (-not (Test-Path -LiteralPath $artifact -PathType Leaf) -or
            (Get-Item -LiteralPath $artifact).Length -eq 0) {
            throw "Resume refused: incomplete cached payload at $artifact (source fingerprint $ExpectedFingerprint)"
        }
    }
    $imageMarker = Join-Path $ArchRoot "proxy-image.ref"
    if (-not (Test-Path -LiteralPath $imageMarker -PathType Leaf)) {
        throw "Resume refused: missing proxy-image.ref (source fingerprint $ExpectedFingerprint)"
    }
    $actual = [IO.File]::ReadAllText($imageMarker, [Text.Encoding]::UTF8).Trim()
    if ($actual -ne $ExpectedImage) {
        throw "Resume refused: cached image '$actual' does not match source fingerprint image '$ExpectedImage'"
    }
}
