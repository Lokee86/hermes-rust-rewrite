# Fetch the pinned standalone CPython that hermes-python-host embeds and the Windows package
# ships (python-build-standalone, install_only_stripped), verify it, and print python.exe's
# path. Build with PYO3_PYTHON set to it:  $env:PYO3_PYTHON = ./scripts/python-runtime.ps1
$ErrorActionPreference = "Stop"
$release = "20261026"
$version = "3.13.18"
$triple = "x86_64-pc-windows-msvc"
$sha = "7369f7a35f261222a6ac8b3a87c1c88eeebee8af945d85346202591a64038dd3"
$root = Join-Path (Join-Path $PSScriptRoot "..") "target/python-runtime/$version-$triple"
$python = Join-Path $root "python/python.exe"
if (!(Test-Path $python)) {
    $archive = "cpython-$version+$release-$triple-install_only_stripped.tar.gz"
    $null = New-Item -ItemType Directory -Path $root -Force
    $file = Join-Path $root $archive
    Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/astral-sh/python-build-standalone/releases/download/$release/$archive" -OutFile $file
    if ((Get-FileHash -Algorithm SHA256 $file).Hash.ToLower() -ne $sha) { throw "Checksum mismatch for $archive" }
    tar -xzf $file -C $root
    if ($LASTEXITCODE -ne 0) { throw "Could not unpack $archive" }
}
(Resolve-Path $python).Path
