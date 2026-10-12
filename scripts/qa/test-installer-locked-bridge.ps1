param([string]$Makensis = "$env:LOCALAPPDATA\tauri\NSIS\makensis.exe")
$ErrorActionPreference = 'Stop'
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('vellum-installer-race-' + [guid]::NewGuid())
$installRoot = Join-Path $testRoot 'install'
$bin = Join-Path $installRoot 'binaries'
New-Item -ItemType Directory -Path $bin | Out-Null
$bridge = Join-Path $bin 'vellum-codex-app-server.exe'
$newImage = Join-Path $testRoot 'new.exe'
$source = Join-Path $testRoot 'fake.cs'
$ready = Join-Path $testRoot 'ready'
$child = $null
$relaunched = $null
$locked = $null
$code = 'using System; using System.IO; using System.Threading; using System.Diagnostics; class Fake { static void Main(string[] args) { string ready = args.Length > 0 ? args[0] : Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "relaunched.txt"); File.WriteAllText(ready, "ready"); if(args.Length == 0) File.WriteAllText(Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "relaunched-pid.txt"), Process.GetCurrentProcess().Id.ToString()); while(true) Thread.Sleep(100); } }'
[IO.File]::WriteAllText($source, $code, [Text.UTF8Encoding]::new($false))
$compiler = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
function Start-Fake {
    Remove-Item -LiteralPath $ready -ErrorAction SilentlyContinue
    $process = Start-Process -FilePath $bridge -ArgumentList ('"' + $ready + '"') -WindowStyle Hidden -PassThru
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while (!(Test-Path -LiteralPath $ready)) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'fake bridge did not start' }
        Start-Sleep -Milliseconds 20
    }
    return $process
}
try {
    & $compiler /nologo /target:winexe "/out:$bridge" $source
    if ($LASTEXITCODE -ne 0) { throw 'compile old fake bridge failed' }
    [IO.File]::WriteAllText($source, $code.Replace('"ready"', '"new-ready"'), [Text.UTF8Encoding]::new($false))
    & $compiler /nologo /target:winexe "/out:$newImage" $source
    if ($LASTEXITCODE -ne 0) { throw 'compile new fake bridge failed' }
    $oldHash = (Get-FileHash -LiteralPath $bridge).Hash
    $newHash = (Get-FileHash -LiteralPath $newImage).Hash
    $child = Start-Fake
    Stop-Process -Id $child.Id
    $child.WaitForExit()
    # Model Desktop immediately restarting the same path after taskkill.
    $child = Start-Fake
    $copyFailed = $false
    try { Copy-Item -LiteralPath $newImage -Destination $bridge -Force } catch { $copyFailed = $true }
    if (!$copyFailed -or (Get-FileHash -LiteralPath $bridge).Hash -ne $oldHash) { throw 'legacy locked overwrite did not reproduce' }

    $hook = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../src-tauri/windows/installer-hooks.nsh'))
    $installer = Join-Path $testRoot 'race-setup.exe'
    $script = Join-Path $testRoot 'race.nsi'
    # Invoke only the production rename macro; never global taskkill hooks.
    $nsis = @"
Unicode true
Name "Vellum isolated race regression"
OutFile "$installer"
RequestExecutionLevel user
!include "$hook"
Function CompileProductionHookOnly
  !insertmacro NSIS_HOOK_PREINSTALL
FunctionEnd
Section
  System::Call 'kernel32::GetTickCount() i.r0'
  System::Call 'kernel32::GetCurrentProcessId() i.r1'
  StrCpy `$R0 `$0
  StrCpy `$R1 `$1
  FileOpen `$R2 "`$INSTDIR\retired-suffix.txt" w
  FileWrite `$R2 "`$R0-`$R1"
  FileClose `$R2
  !insertmacro VELLUM_RETIRE_SIDECARS
  SetOutPath "`$INSTDIR\binaries"
  File /oname=vellum-codex-app-server.exe "$newImage"
  IfErrors 0 +3
  SetErrorLevel 2
  Abort
SectionEnd
"@
    [IO.File]::WriteAllText($script, $nsis, [Text.UTF8Encoding]::new($false))
    & $Makensis /V2 $script
    if ($LASTEXITCODE -ne 0) { throw 'NSIS regression installer did not compile' }
    $setup = Start-Process -FilePath $installer -ArgumentList @('/S', ('/D=' + $installRoot)) -WindowStyle Hidden -Wait -PassThru
    if ($setup.ExitCode -ne 0) { throw "isolated installer failed: $($setup.ExitCode)" }
    if ((Get-FileHash -LiteralPath $bridge).Hash -ne $newHash) { throw 'new bridge was not installed' }
    $suffix = [IO.File]::ReadAllText((Join-Path $installRoot 'retired-suffix.txt'))
    if ($suffix -notmatch '^-?\d+-[1-9]\d*$') { throw "invalid retirement identity: $suffix" }
    if (!(Test-Path -LiteralPath ($bridge + '.old-' + $suffix))) { throw 'old running image was not retired' }
    Stop-Process -Id $child.Id
    $child.WaitForExit()
    Copy-Item -LiteralPath ($bridge + '.old-' + $suffix) -Destination $bridge -Force
    Copy-Item -LiteralPath $newImage -Destination (Join-Path $installRoot 'vellum-proxy-desktop.exe')
    # Unlike an executable image, this handle expressly denies FILE_SHARE_DELETE.
    $locked = [IO.File]::Open($bridge, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    $setup = Start-Process -FilePath $installer -ArgumentList @('/S', ('/D=' + $installRoot)) -WindowStyle Hidden -PassThru
    # -Wait also waits for descendants, but the relaunched desktop must remain alive.
    if (!$setup.WaitForExit(10000)) { throw 'failed installer did not exit' }
    if ($setup.ExitCode -eq 0) { throw 'unrenamable image should stop installation' }
    if ((Get-FileHash -LiteralPath $bridge).Hash -ne $oldHash) { throw 'aborted install changed locked image' }
    $pidFile = Join-Path $installRoot 'relaunched-pid.txt'
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while (!(Test-Path -LiteralPath $pidFile)) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'aborted installer did not relaunch the existing desktop' }
        Start-Sleep -Milliseconds 20
    }
    $relaunched = Get-Process -Id ([int][IO.File]::ReadAllText($pidFile))
    if ($relaunched.HasExited) { throw 'relaunched fake desktop is not running' }
    $locked.Dispose()
    $locked = $null
    Stop-Process -Id $relaunched.Id
    $relaunched.WaitForExit()
    $partialRoot = Join-Path $testRoot 'partial-install'
    $partialBin = Join-Path $partialRoot 'binaries'
    New-Item -ItemType Directory -Path $partialBin | Out-Null
    $partialBridge = Join-Path $partialBin 'vellum-codex-app-server.exe'
    $partialCore = Join-Path $partialBin 'vellum-enhanced-codex.exe'
    Copy-Item -LiteralPath $bridge -Destination $partialBridge
    Copy-Item -LiteralPath $newImage -Destination (Join-Path $partialRoot 'vellum-proxy-desktop.exe')
    [IO.File]::WriteAllBytes($partialCore, [byte[]](1, 2, 3))
    $coreHash = (Get-FileHash -LiteralPath $partialCore).Hash
    $locked = [IO.File]::Open($partialCore, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    $setup = Start-Process -FilePath $installer -ArgumentList @('/S', ('/D=' + $partialRoot)) -WindowStyle Hidden -PassThru
    if (!$setup.WaitForExit(10000)) { throw 'partial-retirement installer did not exit' }
    if ($setup.ExitCode -eq 0) { throw 'second locked image should stop installation' }
    if ((Get-FileHash -LiteralPath $partialBridge).Hash -ne $oldHash) { throw 'first retired bridge was not restored' }
    if ((Get-FileHash -LiteralPath $partialCore).Hash -ne $coreHash) { throw 'second original changed' }
    if (Get-ChildItem -LiteralPath $partialBin -Filter '*.old-*') { throw 'partial retirement left old images behind' }
    $pidFile = Join-Path $partialRoot 'relaunched-pid.txt'
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while (!(Test-Path -LiteralPath $pidFile)) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'partial rollback did not relaunch desktop' }
        Start-Sleep -Milliseconds 20
    }
    $relaunched = Get-Process -Id ([int][IO.File]::ReadAllText($pidFile))
    if ($relaunched.HasExited) { throw 'partial rollback left desktop stopped' }
    Write-Output "PASS: locked overwrite rejected; successful retirement installs new bytes; failed first/second Rename restores originals and relaunches desktop. Artifacts: $testRoot"
} finally {
    if ($locked) { $locked.Dispose() }
    if ($relaunched -and !$relaunched.HasExited) { Stop-Process -Id $relaunched.Id; $relaunched.WaitForExit() }
    if ($child -and !$child.HasExited) { Stop-Process -Id $child.Id; $child.WaitForExit() }
}
