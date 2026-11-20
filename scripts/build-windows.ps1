$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
cargo build --release --bin glide
Write-Host "Built Windows binary: target\release\glide.exe"
