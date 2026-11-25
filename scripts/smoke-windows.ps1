# Start the packaged app and require its window.
param([string]$Archive, [string]$Picture)
$ErrorActionPreference = "Stop"
$work = Join-Path ([System.IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString())
Expand-Archive $Archive $work
$env:HERMES_RUST_DATA_DIR = Join-Path $work "data"
$env:ICED_BACKEND = "tiny-skia"
$app = Start-Process (Join-Path $work "Hermes/Hermes.exe") -PassThru
try {
    foreach ($attempt in 1..60) {
        Start-Sleep -Seconds 1
        $app.Refresh()
        if ($app.HasExited) { throw "Hermes exited ($($app.ExitCode))" }
        if ($app.MainWindowHandle -ne 0) { break }
    }
    if ($app.MainWindowHandle -eq 0) { throw "No window appeared" }
    Start-Sleep -Seconds 8
    $app.Refresh()
    if ($app.HasExited) { throw "Hermes exited after showing its window ($($app.ExitCode))" }
    if (!$app.Responding) { throw "The window does not respond" }
    Write-Host "window: $($app.MainWindowTitle)"
    Add-Type -AssemblyName System.Windows.Forms, System.Drawing
    $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bitmap = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
    $bitmap.Save((Join-Path $PWD $Picture))
    # The installer, silently, into a folder of its own.
    $setup = Join-Path (Split-Path $Archive) "Hermes-windows-AMD64-setup.exe"
    if (Test-Path $setup) {
        $target = Join-Path $work "installed"
        $run = Start-Process $setup "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CURRENTUSER /DIR=`"$target`"" -PassThru -Wait
        if ($run.ExitCode -ne 0) { throw "The installer failed ($($run.ExitCode))" }
        foreach ($file in "Hermes.exe", "hermes-python-host.exe", "python/Lib/os.py") { if (!(Test-Path (Join-Path $target $file))) { throw "The installer left out $file" } }
        $run = Start-Process (Join-Path $target "unins000.exe") "/VERYSILENT /NORESTART" -PassThru -Wait
        Write-Host "installed and uninstalled"
    }
} finally {
    if (!$app.HasExited) { Stop-Process -Id $app.Id -Force }
}
