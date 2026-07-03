$ErrorActionPreference = "Stop"

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $script = $MyInvocation.MyCommand.Path
    Start-Process -FilePath "powershell.exe" -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$script`"" -Verb RunAs
    exit
}

$base = Split-Path -Parent $MyInvocation.MyCommand.Path
$serviceExe = Join-Path $base "rsduck-service.exe"
$serviceName = "rsduck"

Set-Location $base
New-Item -ItemType Directory -Force -Path (Join-Path $base "logs") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $base "snapshot") | Out-Null

if (-not (Test-Path $serviceExe)) {
    throw "rsduck-service.exe not found: $serviceExe"
}

$existing = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($existing) {
    if ($existing.Status -ne "Stopped") {
        Stop-Service -Name $serviceName -ErrorAction SilentlyContinue
        $existing.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(30))
    }
    & sc.exe delete $serviceName | Out-Null
    Start-Sleep -Seconds 2
}

& $serviceExe install
& $serviceExe start

Write-Host "rsduck service installed and started."
