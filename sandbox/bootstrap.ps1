[CmdletBinding()]
param(
    [switch]$FunctionsOnly
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$runId = Get-Date -Format "yyyyMMdd-HHmmss"
$logPath = "C:\Exchange\sandbox-$runId.log"

function Write-Step {
    param([string]$Message)
    $line = "[{0}] {1}" -f (Get-Date -Format "HH:mm:ss"), $Message
    Write-Host $line -ForegroundColor Cyan
    Add-Content -LiteralPath $logPath -Value $line -Encoding UTF8
}

function Find-VellumExecutable {
    $roots = @(
        (Join-Path $env:LOCALAPPDATA "Vellum"),
        (Join-Path $env:LOCALAPPDATA "Programs"),
        $env:LOCALAPPDATA,
        "C:\Program Files"
    )

    foreach ($root in $roots) {
        if (-not (Test-Path $root)) {
            continue
        }

        $candidate = Get-ChildItem -LiteralPath $root -Recurse -File -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -ieq "Vellum.exe" } |
            Select-Object -First 1
        if ($candidate) {
            return $candidate.FullName
        }
    }

    return $null
}

function Get-WebView2Version {
    $clientId = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
    $keys = @(
        "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\$clientId",
        "HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients\$clientId",
        "HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\$clientId"
    )

    foreach ($key in $keys) {
        $version = (Get-ItemProperty -LiteralPath $key -Name "pv" -ErrorAction SilentlyContinue).pv
        if ($version -and $version -ne "0.0.0.0") {
            return $version
        }
    }

    return $null
}

function Install-WebView2Runtime {
    $installedVersion = Get-WebView2Version
    if ($installedVersion) {
        Write-Step "Microsoft Edge WebView2 Runtime already available: $installedVersion"
        return
    }

    $bootstrapper = Join-Path $env:TEMP "MicrosoftEdgeWebview2Setup.exe"
    $downloadUrl = "https://go.microsoft.com/fwlink/p/?LinkId=2124703"
    Write-Step "Downloading the official Microsoft WebView2 Evergreen bootstrapper"
    Invoke-WebRequest -Uri $downloadUrl -OutFile $bootstrapper -UseBasicParsing

    Write-Step "Installing Microsoft Edge WebView2 Runtime"
    $install = Start-Process -FilePath $bootstrapper `
        -ArgumentList @("/silent", "/install") `
        -Wait `
        -PassThru
    Remove-Item -LiteralPath $bootstrapper -Force -ErrorAction SilentlyContinue

    $installedVersion = Get-WebView2Version
    if (-not $installedVersion) {
        throw "WebView2 Runtime installation failed (exit code $($install.ExitCode))."
    }

    Write-Step "Microsoft Edge WebView2 Runtime installed: $installedVersion"
}

function Show-ExchangeFolder {
    $explorer = Get-Command explorer.exe -ErrorAction SilentlyContinue
    if (-not $explorer) {
        Write-Step "No optional file viewer is available; open C:\Exchange manually"
        return
    }

    try {
        Start-Process -FilePath $explorer.Source -ArgumentList "C:\Exchange"
    }
    catch {
        Write-Step "Could not open C:\Exchange automatically; this does not affect bootstrap success"
    }
}

function Get-WinGetCommand {
    $command = Get-Command winget.exe -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    $alias = Join-Path $env:LOCALAPPDATA "Microsoft\WindowsApps\winget.exe"
    if (Test-Path $alias) {
        return $alias
    }

    $package = Get-AppxPackage -Name Microsoft.DesktopAppInstaller -ErrorAction SilentlyContinue
    if ($package) {
        $packagedExe = Join-Path $package.InstallLocation "winget.exe"
        if (Test-Path $packagedExe) {
            return $packagedExe
        }
    }

    return $null
}

function Install-WinGetForSandbox {
    $winget = Get-WinGetCommand
    if ($winget) {
        Write-Step "WinGet already available: $winget"
        return $winget
    }

    Write-Step "WinGet is absent; bootstrapping it with Microsoft's official Sandbox procedure"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Install-PackageProvider -Name NuGet -Force | Out-Null
    Install-Module -Name Microsoft.WinGet.Client `
        -Force `
        -Repository PSGallery `
        -Scope AllUsers `
        -AllowClobber
    Import-Module Microsoft.WinGet.Client -Force
    Repair-WinGetPackageManager -AllUsers

    $windowsApps = Join-Path $env:LOCALAPPDATA "Microsoft\WindowsApps"
    if ($env:PATH -notlike "*$windowsApps*") {
        $env:PATH = "$windowsApps;$env:PATH"
    }

    $winget = Get-WinGetCommand
    if (-not $winget) {
        throw "WinGet bootstrap completed but winget.exe is still unavailable."
    }

    Write-Step "WinGet installed: $winget"
    return $winget
}

function Get-ChatGPTPackage {
    $package = Get-AppxPackage -ErrorAction SilentlyContinue |
        Where-Object {
            $_.Name -match "OpenAI|ChatGPT|Codex" -or
            $_.PackageFullName -match "OpenAI|ChatGPT|Codex"
        } |
        Select-Object -First 1
    return $package
}

function Invoke-WinGetLogged {
    param(
        [string]$WinGet,
        [string[]]$Arguments
    )

    $output = & $WinGet @Arguments 2>&1
    $exitCode = $LASTEXITCODE
    foreach ($line in $output) {
        $text = $line.ToString()
        Write-Host $text
        Add-Content -LiteralPath $logPath -Value "[winget] $text" -Encoding UTF8
    }

    return [pscustomobject]@{
        ExitCode = $exitCode
        Output = @($output | ForEach-Object { $_.ToString() })
    }
}

function Export-WinGetDiagnostics {
    $diagnosticDirectory = Join-Path $env:LOCALAPPDATA `
        "Packages\Microsoft.DesktopAppInstaller_8wekyb3d8bbwe\LocalState\DiagOutputDir"
    if (-not (Test-Path $diagnosticDirectory)) {
        return
    }

    $destination = "C:\Exchange\winget-diagnostics"
    New-Item -ItemType Directory -Force -Path $destination | Out-Null
    Get-ChildItem -LiteralPath $diagnosticDirectory -Filter "*.log" -File `
        -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 5 |
        Copy-Item -Destination $destination -Force
}

function Initialize-StoreMarket {
    # Windows Sandbox can start without a home region. WinGet then calls the
    # Microsoft Store API with Market=ZZ, which the Store rejects with
    # APPINSTALLER_CLI_ERROR_RESTAPI_INTERNAL_ERROR (0x8A15003B).
    $storeGeoId = 244
    $storeRegion = "US"
    $storeLocale = "en-US"

    Write-Step "Setting the disposable Sandbox Store market to $storeRegion ($storeLocale)"
    Set-WinHomeLocation -GeoId $storeGeoId
    Set-Culture -CultureInfo $storeLocale

    # Keep this PowerShell process consistent as well. winget.exe is launched
    # as a new process and reads the home location set above.
    [System.Threading.Thread]::CurrentThread.CurrentCulture =
        [System.Globalization.CultureInfo]::GetCultureInfo($storeLocale)
    [System.Threading.Thread]::CurrentThread.CurrentUICulture =
        [System.Globalization.CultureInfo]::GetCultureInfo($storeLocale)

    $homeLocation = Get-WinHomeLocation
    Write-Step "Sandbox Store market ready: GeoId=$($homeLocation.GeoId), Region=$($homeLocation.HomeLocation)"
}

function Install-ChatGPTApp {
    $existing = Get-ChatGPTPackage
    if ($existing) {
        Write-Step "ChatGPT/Codex package already installed: $($existing.PackageFullName)"
        return $existing
    }

    Initialize-StoreMarket
    $winget = Install-WinGetForSandbox
    Write-Step "Resetting and updating WinGet sources before Store installation"
    $sourceReset = Invoke-WinGetLogged `
        -WinGet $winget `
        -Arguments @("source", "reset", "--force", "--disable-interactivity")
    if ($sourceReset.ExitCode -ne 0) {
        Write-Step "WinGet source reset returned $($sourceReset.ExitCode); continuing with source update"
    }
    $sourceUpdate = Invoke-WinGetLogged `
        -WinGet $winget `
        -Arguments @("source", "update", "--name", "msstore", "--disable-interactivity")
    if ($sourceUpdate.ExitCode -ne 0) {
        Write-Step "WinGet msstore source update returned $($sourceUpdate.ExitCode)"
    }

    Write-Step "Installing the official ChatGPT/Codex Microsoft Store package"
    $install = $null
    foreach ($attempt in 1..3) {
        Write-Step "ChatGPT/Codex Store install attempt $attempt of 3"
        $install = Invoke-WinGetLogged `
            -WinGet $winget `
            -Arguments @(
            "install", "--exact", "--id", "9PLM9XGG6VKS",
            "--source", "msstore", "--accept-source-agreements",
            "--accept-package-agreements", "--silent", "--disable-interactivity"
        )

        $package = Get-ChatGPTPackage
        if ($package) {
            break
        }

        if ($attempt -lt 3) {
            $delay = 10 * $attempt
            Write-Step "Store attempt failed with $($install.ExitCode); retrying in $delay seconds"
            Start-Sleep -Seconds $delay
            Invoke-WinGetLogged `
                -WinGet $winget `
                -Arguments @("source", "update", "--name", "msstore", "--disable-interactivity") |
                Out-Null
        }
    }

    $package = Get-ChatGPTPackage
    if (-not $package) {
        Export-WinGetDiagnostics
        throw "ChatGPT/Codex Store installation failed (winget exit code $($install.ExitCode))."
    }

    Write-Step "ChatGPT/Codex installed: $($package.PackageFullName)"
    return $package
}

function Start-ChatGPTApp {
    param($Package)

    try {
        $manifest = Get-AppxPackageManifest -Package $Package.PackageFullName
        $applicationId = @($manifest.Package.Applications.Application)[0].Id
        $appUserModelId = "$($Package.PackageFamilyName)!$applicationId"
        Write-Step "Launching ChatGPT/Codex: $appUserModelId"
        Start-Process -FilePath "explorer.exe" -ArgumentList "shell:AppsFolder\$appUserModelId"
    }
    catch {
        Write-Step "ChatGPT/Codex is installed but could not be auto-launched; open it from Start"
    }
}

if ($FunctionsOnly) {
    return
}

New-Item -ItemType Directory -Force -Path "C:\Exchange" | Out-Null
Set-Content -LiteralPath $logPath -Value "Vellum onboarding sandbox bootstrap" -Encoding UTF8

try {
    # Onboarding is a first-run flow, so every state root Vellum reads must start
    # empty. A fresh Sandbox profile is already empty; pinning CODEX_HOME here
    # keeps the isolated home explicit and inspectable from C:\Exchange.
    $sandboxHome = "C:\IsolatedHome"
    $codexHome = Join-Path $sandboxHome ".codex"
    New-Item -ItemType Directory -Force -Path $codexHome | Out-Null

    $env:CODEX_HOME = $codexHome
    $env:CODEX_SQLITE_HOME = $codexHome
    [Environment]::SetEnvironmentVariable("CODEX_HOME", $codexHome, "User")
    [Environment]::SetEnvironmentVariable("CODEX_SQLITE_HOME", $codexHome, "User")
    Write-Step "Created isolated Codex home under $codexHome"

    Install-WebView2Runtime

    $setup = Get-ChildItem -LiteralPath "C:\Installers" -Filter "*-setup.exe" -File |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
    if (-not $setup) {
        throw "No Vellum NSIS installer was found in C:\Installers. Run 'pnpm tauri build' on the host first."
    }

    # Tauri's NSIS installer takes /S for silent. installMode is currentUser, so
    # this needs no elevation and lands under %LOCALAPPDATA%.
    Write-Step "Installing $($setup.Name)"
    $install = Start-Process -FilePath $setup.FullName `
        -ArgumentList @("/S") `
        -Wait `
        -PassThru
    if ($install.ExitCode -ne 0) {
        throw "Vellum installer failed with exit code $($install.ExitCode)."
    }
    Write-Step "Vellum installation completed"

    $vellumExe = Find-VellumExecutable
    if (-not $vellumExe) {
        throw "Vellum installed but Vellum.exe was not found under %LOCALAPPDATA% or Program Files."
    }
    Write-Step "Vellum installed at: $vellumExe"
    Set-Content -LiteralPath "C:\Exchange\vellum-path.txt" -Value $vellumExe -Encoding UTF8

    # Codex goes in before Vellum launches, so the onboarding screen's Codex
    # detection sees the real app on its first probe instead of a stale "missing".
    $chatGptPackage = Install-ChatGPTApp
    Start-ChatGPTApp -Package $chatGptPackage

    Write-Step "Launching Vellum for the onboarding flow"
    Start-Process -FilePath $vellumExe

    Copy-Item -LiteralPath "C:\SandboxKit\TEST-CHECKLIST.md" `
        -Destination "C:\Exchange\TEST-CHECKLIST.md" `
        -Force
    Remove-Item -LiteralPath "C:\Exchange\BOOTSTRAP-FAILED.txt" `
        -Force `
        -ErrorAction SilentlyContinue
    Write-Step "Bootstrap complete. Results written to C:\Exchange"
    Show-ExchangeFolder
}
catch {
    Write-Step "BOOTSTRAP FAILED: $($_.Exception.Message)"
    Set-Content -LiteralPath "C:\Exchange\BOOTSTRAP-FAILED.txt" `
        -Value ($_ | Out-String) `
        -Encoding UTF8
    Write-Host "See $logPath and C:\Exchange\BOOTSTRAP-FAILED.txt for details." -ForegroundColor Yellow
    exit 1
}
