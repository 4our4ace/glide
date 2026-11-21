param(
    [string]$ExePath = "target\release\glide.exe",
    [string]$OutPath = "dist\glide-windows-x86_64-installer.msi"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Test-Path $ExePath)) {
    throw "Windows executable not found at '$ExePath'. Run .\scripts\build-windows.ps1 first."
}

$ver = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$wix = Join-Path $env:USERPROFILE ".dotnet\tools\wix.exe"
if (-not (Test-Path $wix)) {
    dotnet tool install --global wix --version 5.0.2
}

$outDir = Split-Path -Parent $OutPath
if ($outDir) {
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
}

& $wix build packaging\windows\glide.wxs `
    -arch x64 `
    -d "Version=$ver" `
    -d "ExePath=$ExePath" `
    -d "IconPath=packaging\windows\glide.ico" `
    -o $OutPath

if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

Remove-Item -Force -ErrorAction SilentlyContinue ([System.IO.Path]::ChangeExtension($OutPath, ".wixpdb"))
Write-Host "Built Windows MSI installer: $OutPath"
