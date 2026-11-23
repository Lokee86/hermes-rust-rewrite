# Stage the pinned Windows Herdr Core fixture and its ConPTY runtime.
param([string]$Destination = (Join-Path $PSScriptRoot '../target/herdr-core-0.1.22'))
$ErrorActionPreference = 'Stop'
$null = New-Item -ItemType Directory -Path $Destination -Force
$Destination = (Resolve-Path -LiteralPath $Destination).Path
$archive = Join-Path $Destination 'herdr-core-windows-x86_64.zip'
$url = 'https://releases.herdr.dev/core/0.1.22/herdr-core-windows-x86_64.zip'
curl.exe -fsSL $url -o $archive
if ($LASTEXITCODE) { throw 'Could not download Herdr Core' }
$stream = [IO.File]::OpenRead($archive)
try { $sha256 = [Security.Cryptography.SHA256]::Create(); $hash = [BitConverter]::ToString($sha256.ComputeHash($stream)).Replace('-','') }
finally { $stream.Dispose(); $sha256.Dispose() }
if ($hash -ne '37cae9d6c522e205efcdb55a42d354285f41b8cbb4c6e60fb505b73b151966c9') { throw 'Herdr Core 0.1.22 checksum mismatch' }
& "$env:SystemRoot\System32\tar.exe" -xf $archive -C $Destination
if ($LASTEXITCODE) { throw 'Could not extract Herdr Core' }
$exe = Join-Path $Destination 'herdr-core.exe'
if ((& $exe --version) -ne 'hc 0.1.22') { throw 'Unexpected Herdr Core version' }
Write-Output $exe
