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
$code = 'using System; using System.IO; using System.Threading; class Fake { static void Main(string[] args) { File.WriteAllText(args[0], "ready"); while(true) Thread.Sleep(100); } }'
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
    & $compiler /nologo /target:exe "/out:$bridge" $source
    if ($LASTEXITCODE -ne 0) { throw 'compile old fake bridge failed' }
    [IO.File]::WriteAllText($source, $code.Replace('"ready"', '"new-ready"'), [Text.UTF8Encoding]::new($false))
    & $compiler /nologo /target:exe "/out:$newImage" $source
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
  !insertmacro VELLUM_RETIRE_IMAGE "binaries\vellum-codex-app-server.exe"
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
    Write-Output "PASS: locked legacy overwrite rejected; production Rename macro installed new bytes. Artifacts: $testRoot"
} finally {
    if ($child -and !$child.HasExited) { Stop-Process -Id $child.Id; $child.WaitForExit() }
}
