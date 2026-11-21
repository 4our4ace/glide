$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
$env:RUSTFLAGS = if ($env:RUSTFLAGS) { "$env:RUSTFLAGS -C target-feature=+crt-static" } else { "-C target-feature=+crt-static" }
cargo build --release --locked --bin glide
Write-Host "Built Windows binary (static CRT): target\release\glide.exe"
