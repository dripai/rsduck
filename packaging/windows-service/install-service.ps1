$ErrorActionPreference = "Stop"

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $script = $MyInvocation.MyCommand.Path
    Start-Process -FilePath "powershell.exe" -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$script`"" -Verb RunAs
    exit
}

$base = Split-Path -Parent $MyInvocation.MyCommand.Path
$serviceExe = Join-Path $base "rsduck-service.exe"

Set-Location $base
New-Item -ItemType Directory -Force -Path (Join-Path $base "logs") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $base "snapshot") | Out-Null

if (-not (Test-Path $serviceExe)) {
    throw "rsduck-service.exe not found: $serviceExe"
}

& $serviceExe install
& $serviceExe start

Write-Host "rsduck service installed and started."
