# Guest bootstrap inside Windows Sandbox. Reads C:\QaOutput\run-manifest.json.
# Does not pick the newest installer by mtime — only the named file + SHA.
[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$out = "C:\QaOutput"
$manifestPath = Join-Path $out "run-manifest.json"
New-Item -ItemType Directory -Force -Path $out | Out-Null
New-Item -ItemType Directory -Force -Path "C:\Exchange" | Out-Null
New-Item -ItemType Directory -Force -Path "C:\QaTools" | Out-Null

function Write-Utf8Json([string]$Path, $Object) {
    $json = $Object | ConvertTo-Json -Compress -Depth 8
    [System.IO.File]::WriteAllText($Path, $json)
}

function Write-Heartbeat([string]$Stage) {
    Write-Utf8Json (Join-Path $out "HEARTBEAT") @{ stage = $Stage; at = (Get-Date).ToUniversalTime().ToString("o") }
}

function Get-FileSha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-ProcessSnapshot {
    @(Get-Process -ErrorAction SilentlyContinue | Select-Object Name, Id)
}

try {
    Write-Heartbeat "start"
    if (-not (Test-Path -LiteralPath $manifestPath)) { throw "run-manifest.json missing" }
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    $installerName = $manifest.installerName
    $expectedSha = "$($manifest.installerSha256)".ToLowerInvariant()
    $mappedSetup = Join-Path "C:\Installers" $installerName
    if (-not (Test-Path -LiteralPath $mappedSetup)) { throw "named installer missing: $installerName" }
    $actual = Get-FileSha256 $mappedSetup
    if ($actual -ne $expectedSha) { throw "installer SHA mismatch expected $expectedSha got $actual" }
    # Mapped-folder executables are blocked by WDAC/Application Control in Sandbox.
    $setup = Join-Path "C:\QaTools" $installerName
    Copy-Item -LiteralPath $mappedSetup -Destination $setup -Force
    Unblock-File -LiteralPath $setup -ErrorAction SilentlyContinue
    $copiedSha = Get-FileSha256 $setup
    if ($copiedSha -ne $expectedSha) {
        throw "copied installer SHA mismatch expected $expectedSha got $copiedSha (mapped copy may be truncated)"
    }

    Write-Heartbeat "unwrap-credentials"
    $node = "C:\SandboxKit\node.exe"
    $openJs = "C:\SandboxKit\open-credentials.mjs"
    $onceKey = "C:\QaOnce\once.key"
    if ((Test-Path $node) -and (Test-Path $openJs) -and (Test-Path "C:\QaSecrets\credential-envelope.bin") -and (Test-Path $onceKey)) {
        & $node $openJs "C:\QaSecrets" "C:\QaTools" $out $onceKey
    }
    Remove-Item -LiteralPath $onceKey -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $out "once.key") -Force -ErrorAction SilentlyContinue

    $codexHome = "C:\IsolatedHome\.codex"
    New-Item -ItemType Directory -Force -Path $codexHome | Out-Null
    $env:CODEX_HOME = $codexHome
    $env:CODEX_SQLITE_HOME = $codexHome
    [Environment]::SetEnvironmentVariable("CODEX_HOME", $codexHome, "User")
    [Environment]::SetEnvironmentVariable("CODEX_SQLITE_HOME", $codexHome, "User")

    $kit = "C:\SandboxKit\bootstrap.ps1"
    if (-not (Test-Path -LiteralPath $kit)) { throw "sandbox kit bootstrap.ps1 missing" }
    . $kit -FunctionsOnly

    $scenario = "$($manifest.scenario)"
    if (-not $scenario) { $scenario = "vellum-first" }

    Write-Heartbeat "webview2"
    Install-WebView2Runtime
    $wv = Get-WebView2Version
    Write-Utf8Json (Join-Path $out "webview2.json") @{
        webview2 = $wv
        source = "https://go.microsoft.com/fwlink/p/?LinkId=2124703"
        at = (Get-Date).ToUniversalTime().ToString("o")
    }

    function Install-VellumRecorded {
        Write-Heartbeat "install-vellum"
        $install = $null
        try {
            $install = Start-Process -FilePath $setup -ArgumentList @("/S") -Wait -PassThru
        } catch {
            $cmd = Start-Process -FilePath "cmd.exe" -ArgumentList @("/c", "`"$setup`" /S") -Wait -PassThru
            $install = $cmd
        }
        $exe = Find-VellumExecutable
        $record = @{
            step = "vellum"
            exitCode = $install.ExitCode
            sha256 = $actual
            name = $installerName
            exe = $exe
            at = (Get-Date).ToUniversalTime().ToString("o")
        }
        Write-Utf8Json (Join-Path $out "install-vellum.json") $record
        if ($install.ExitCode -ne 0) { throw "Vellum installer exit $($install.ExitCode)" }
        if (-not $exe) { throw "Vellum.exe missing after install" }
        return $record
    }

    function Install-CodexRecorded {
        Write-Heartbeat "install-codex"
        $before = Get-ChatGPTPackage
        $errorText = $null
        $pkg = $null
        try {
            $pkg = Install-ChatGPTApp
        } catch {
            $errorText = $_.ToString()
        }
        $after = Get-ChatGPTPackage
        $record = @{
            step = "codex"
            ok = [bool]$after
            presentBefore = [bool]$before
            packageFullName = if ($after) { $after.PackageFullName } else { $null }
            version = if ($after) { "$($after.Version)" } else { $null }
            source = "msstore:9PLM9XGG6VKS"
            error = $errorText
            at = (Get-Date).ToUniversalTime().ToString("o")
        }
        Write-Utf8Json (Join-Path $out "install-codex.json") $record
        if (-not $after) { throw "Codex Store installation failed: $errorText" }
        return $record
    }

    $codexBefore = Get-ChatGPTPackage
    Write-Utf8Json (Join-Path $out "codex-before.json") @{
        present = [bool]$codexBefore
        scenario = $scenario
        at = (Get-Date).ToUniversalTime().ToString("o")
    }

    if ($scenario -eq "codex-first") {
        Install-CodexRecorded | Out-Null
        Install-VellumRecorded | Out-Null
    } else {
        if ($codexBefore) { throw "vellum-first requires Codex to be absent; found $($codexBefore.PackageFullName)" }
        Install-VellumRecorded | Out-Null
        Install-CodexRecorded | Out-Null
    }

    Write-Heartbeat "credentials"
    $human = @{
        checkpoints = @()
        injectedFields = @()
        storedVia = "not-stored"
    }
    $opened = $null
    $openedPath = "C:\QaTools\opened.json"
    if (Test-Path $openedPath) {
        $opened = Get-Content $openedPath -Raw | ConvertFrom-Json
        if ($opened.opencode) { $human.injectedFields += "opencode" }
        if ($opened.qwen) { $human.injectedFields += "qwen" }
        if ($opened.grok) { $human.checkpoints += "grok-interactive-login" }
        $human.checkpoints += "codex-interactive-login"
    } else {
        $human.checkpoints += "no-envelope"
    }
    Write-Utf8Json (Join-Path $out "human-checkpoints.json") $human

    Write-Heartbeat "launch-vellum"
    $vellumExe = Find-VellumExecutable
    if ($vellumExe) {
        $env:CODEX_HOME = $codexHome
        Start-Process -FilePath $vellumExe
        Start-Sleep -Seconds 8
    }
    Write-Utf8Json (Join-Path $out "onboarding-launch.json") @{
        exe = $vellumExe
        processes = @(Get-ProcessSnapshot)
        isolatedCodexHome = $codexHome
        at = (Get-Date).ToUniversalTime().ToString("o")
    }

    if (Test-Path "C:\SandboxKit\desktop\screenshot.ps1") {
        & "C:\SandboxKit\desktop\screenshot.ps1" -OutFile (Join-Path $out "onboarding.png") -ErrorAction SilentlyContinue
    }

    Write-Heartbeat "store-credentials"
    if ($vellumExe -and $opened -and $opened.opencode) {
        $act = "C:\SandboxKit\desktop\act.ps1"
        $addName = $null
        if (Test-Path "C:\SandboxKit\drivers.json") {
            $drv = Get-Content -LiteralPath "C:\SandboxKit\drivers.json" -Encoding UTF8 -Raw | ConvertFrom-Json
            $addName = $drv."tab.today.add-provider".action.control
        }
        if ((Test-Path $act) -and $addName) {
            & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $act -Name $addName -ProcessName "Vellum" -ControlType "Button" -ErrorAction SilentlyContinue
            Start-Sleep -Seconds 2
            $tmpKey = "C:\QaTools\set-value.tmp"
            [System.IO.File]::WriteAllText($tmpKey, "$($opened.opencode)")
            & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $act -Name "*" -ProcessName "Vellum" -ControlType "Edit" -SetValueFile $tmpKey -ErrorAction SilentlyContinue
            Remove-Item -LiteralPath $tmpKey -Force -ErrorAction SilentlyContinue
            $saveName = $drv."tab.models.add-provider-save".action.control
            if ($saveName) {
                & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $act -Name $saveName -ProcessName "Vellum" -ControlType "Button" -ErrorAction SilentlyContinue
            }
            Start-Sleep -Seconds 2
        }
        Remove-Item -LiteralPath "C:\QaTools\opened.json" -Force -ErrorAction SilentlyContinue
        $credRoot = @(
            (Join-Path $env:APPDATA "com.vellum.desktop\credentials"),
            (Join-Path $env:LOCALAPPDATA "com.vellum.desktop\credentials")
        ) | Where-Object { Test-Path $_ } | Select-Object -First 1
        $bins = @()
        if ($credRoot) { $bins = @(Get-ChildItem -LiteralPath $credRoot -Filter "*.bin" -ErrorAction SilentlyContinue) }
        if ($bins.Count -gt 0) {
            $human.storedVia = "product-credentials-dir"
        } else {
            $human.storedVia = "product-ui-attempted-no-credential-file"
            $human.checkpoints += "opencode-key-not-saved-through-product-path"
        }
        Write-Utf8Json (Join-Path $out "human-checkpoints.json") $human
        Write-Utf8Json (Join-Path $out "credential-fields.json") @{
            fields = @($human.injectedFields)
            storedVia = $human.storedVia
        }
    } elseif (-not $vellumExe) {
        $human.storedVia = "not-stored-install-failed"
        Write-Utf8Json (Join-Path $out "human-checkpoints.json") $human
    }

    Write-Heartbeat "drive"
    if (Test-Path "C:\SandboxKit\guest-drive.ps1") {
        & powershell.exe -NoProfile -ExecutionPolicy Bypass -File "C:\SandboxKit\guest-drive.ps1" -ErrorAction SilentlyContinue
    }
    if (Test-Path "C:\SandboxKit\guest-remote.ps1") {
        & powershell.exe -NoProfile -ExecutionPolicy Bypass -File "C:\SandboxKit\guest-remote.ps1" -ErrorAction SilentlyContinue
    }
    if (Test-Path "C:\SandboxKit\guest-live.ps1") {
        & powershell.exe -NoProfile -ExecutionPolicy Bypass -File "C:\SandboxKit\guest-live.ps1" -ErrorAction SilentlyContinue
    }

    if ("$($manifest.uninstall)" -eq "true") {
        Write-Heartbeat "uninstall"
        $uninst = Get-ChildItem -LiteralPath $env:LOCALAPPDATA -Recurse -Filter "*.exe" -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match "Uninstall" -and $_.FullName -match "Vellum" } |
            Select-Object -First 1
        $unExit = $null
        if ($uninst) {
            $un = Start-Process -FilePath $uninst.FullName -ArgumentList @("/S") -Wait -PassThru
            $unExit = $un.ExitCode
        }
        Start-Sleep -Seconds 3
        Write-Utf8Json (Join-Path $out "uninstall-log.json") @{
            ran = [bool]$uninst
            uninstaller = if ($uninst) { $uninst.FullName } else { $null }
            exitCode = $unExit
            vellumExeRemaining = [bool](Find-VellumExecutable)
            processes = @((Get-ProcessSnapshot | Where-Object { $_.Name -match "Vellum|vellum-proxy" }))
            at = (Get-Date).ToUniversalTime().ToString("o")
        }
    }

    Write-Heartbeat "done"
    Write-Utf8Json (Join-Path $out "DONE.json") @{
        ok = $true
        scenario = $scenario
        installerSha256 = $actual
        development = [bool]$manifest.development
        isolatedCodexHome = $codexHome
        webview2 = $wv
        vellumExe = $vellumExe
        humanCheckpoints = $human.checkpoints
    }
}
catch {
    Write-Heartbeat "failed"
    [System.IO.File]::WriteAllText((Join-Path $out "BOOTSTRAP-FAILED.txt"), ($_ | Out-String))
    Remove-Item -LiteralPath "C:\QaOnce\once.key" -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $out "once.key") -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath "C:\QaTools\opened.json" -Force -ErrorAction SilentlyContinue
    Write-Utf8Json (Join-Path $out "guest-live.json") @{
        cases = @()
        note = "install failed; live sessions were not started; env flags are not PASS"
        error = $_.ToString()
    }
    Write-Utf8Json (Join-Path $out "remote-ui.json") @{
        buttons = @()
        note = "install failed; Remote Manager was not driven"
        error = $_.ToString()
    }
    Write-Utf8Json (Join-Path $out "DONE.json") @{ ok = $false; error = $_.ToString() }
    exit 1
}
