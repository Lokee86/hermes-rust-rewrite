param([string]$Destination = "")
$ErrorActionPreference = "Stop"
Push-Location (Join-Path $PSScriptRoot "..")
try {
    if (!$Destination) { $Destination = Join-Path $PWD "dist" }
    $stage = Join-Path ([System.IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString())
    $root = Join-Path $stage "Hermes"
    $null = New-Item -ItemType Directory -Path "$root/Notices" -Force
    $null = New-Item -ItemType Directory -Path $Destination -Force
    cargo build --locked --release --features embedded-herdr -p hermes-desktop
    if ($LASTEXITCODE -ne 0) { throw "Cargo build failed ($LASTEXITCODE)" }
    $meta = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw "Cargo metadata failed" }
    Copy-Item "$($meta.target_directory)/release/hermes-desktop.exe" "$root/Hermes.exe"
    $fixture = & (Join-Path $PSScriptRoot "prepare-herdr.ps1")
    $fixtureDir = Split-Path -Parent $fixture
    Copy-Item (Join-Path $fixtureDir "conpty") $root -Recurse
    Copy-Item (Join-Path $fixtureDir "THIRD-PARTY-NOTICES") "$root/Notices/Herdr-THIRD-PARTY-NOTICES" -Recurse
    Copy-Item (Join-Path $fixtureDir "LICENSE") "$root/Notices/Herdr-LICENSE"
    Copy-Item "assets/icon.ico" "$root/Hermes.ico"
    Copy-Item "LICENSE-HERMES-AGENT" "$root/Notices/Hermes-LICENSE"
    Get-ChildItem "crates/desktop/assets/fonts" -File | Where-Object { $_.Name -match '(NOTICE|LICENSE).*\.txt$' } | Copy-Item -Destination "$root/Notices"
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = Join-Path $stage "Hermes.zip"
    [System.IO.Compression.ZipFile]::CreateFromDirectory(
        $root, $archive, [System.IO.Compression.CompressionLevel]::Optimal, $true)
    Copy-Item -LiteralPath $archive -Destination (Join-Path $Destination "Hermes-windows-$env:PROCESSOR_ARCHITECTURE.zip") -Force
} finally {
    if ($stage -and (Test-Path $stage)) { Remove-Item -LiteralPath $stage -Recurse -Force }
    Pop-Location
}
