#Requires -Version 5
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

cargo build -p sentry-svc --release
if ($LASTEXITCODE -ne 0) { exit 1 }

$targetDir = (cargo metadata --no-deps --format-version 1 | ConvertFrom-Json).target_directory
$src = Join-Path $targetDir 'release\sentry-svc.exe'
$dst = Join-Path $PSScriptRoot 'bin\sentry-svc.exe'

New-Item -ItemType Directory -Force -Path (Split-Path $dst) | Out-Null
Copy-Item $src $dst -Force
Write-Host "Copied $src -> $dst"
