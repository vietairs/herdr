# Verify half of the `Windows ConPTY package` CI job; invoked by windows-conpty-package.sh from the repo root.
$ErrorActionPreference = "Stop"

$runId = "$env:GITHUB_RUN_ID-$env:GITHUB_RUN_ATTEMPT"
$tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }

# --- Verify invalid bundle is rejected and system override recovers
Remove-Item Env:HERDR_SOCKET_PATH, Env:HERDR_CLIENT_SOCKET_PATH -ErrorAction SilentlyContinue
$exe = Join-Path $PWD "target\x86_64-pc-windows-msvc\debug\herdr.exe"
$bundle = Join-Path (Split-Path -Parent $exe) "conpty"
New-Item -ItemType Directory -Force -Path (Join-Path $bundle "x64"), (Join-Path $bundle "arm64") | Out-Null
Set-Content -LiteralPath (Join-Path $bundle "herdr-conpty.json") -Value "{}" -Encoding ascii
[System.IO.File]::WriteAllBytes((Join-Path $bundle "conpty.dll"), [byte[]](0x48, 0x45, 0x52, 0x44, 0x52))
[System.IO.File]::WriteAllBytes((Join-Path $bundle "x64\OpenConsole.exe"), [byte[]](0x48, 0x45, 0x52, 0x44, 0x52))
[System.IO.File]::WriteAllBytes((Join-Path $bundle "arm64\OpenConsole.exe"), [byte[]](0x48, 0x45, 0x52, 0x44, 0x52))
try {
  $rejected = $false
  try {
    .\scripts\windows_smoke_conpty_path.ps1 `
      -ExePath $exe `
      -Session "ci-conpty-invalid-windows-2022-$runId"
  } catch {
    if ($_.Exception.Message -notlike "workspace create failed with exit code*") {
      throw
    }
    $rejected = $true
  }
  if (-not $rejected) {
    throw "Herdr accepted a tampered app-local ConPTY bundle"
  }

  $env:HERDR_WINDOWS_CONPTY = "system"
  .\scripts\windows_smoke_conpty_path.ps1 `
    -ExePath $exe `
    -Session "ci-conpty-system-windows-2022-$runId"
} finally {
  Remove-Item Env:HERDR_WINDOWS_CONPTY -ErrorAction SilentlyContinue
  Remove-Item -LiteralPath $bundle -Recurse -Force -ErrorAction SilentlyContinue
}

# --- Build and verify official ConPTY package
$package = Join-Path $tempRoot "Microsoft.Windows.Console.ConPTY.nupkg"
$stage = Join-Path $tempRoot "herdr-windows-x86_64"
New-Item -ItemType Directory -Force -Path artifacts | Out-Null
.\scripts\package_windows_conpty.ps1 `
  -HerdrExe target\x86_64-pc-windows-msvc\debug\herdr.exe `
  -PackagePath $package `
  -StageDir $stage `
  -OutputPath artifacts\herdr-windows-x86_64.zip

# --- Probe enhanced pane input with bundled ConPTY
$exe = Join-Path $stage "herdr.exe"
$consoleHost = Join-Path $stage "conpty\x64\OpenConsole.exe"
.\scripts\windows_conpty_enhanced_input_probe.ps1 `
  -ExePath $exe `
  -Session "ci-conpty-bundled-windows-2022-$runId" `
  -ExpectedConsoleHostPath $consoleHost

# --- Test packaged installer and repair with Windows PowerShell 5.1
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File `
  .\scripts\windows_install_conpty_package_test.ps1 `
  -ArchivePath artifacts\herdr-windows-x86_64.zip
if ($LASTEXITCODE -ne 0) {
  throw "Windows PowerShell installer test failed with exit code $LASTEXITCODE"
}
