#requires -Version 5.1
<#
.SYNOPSIS
    Fetch ffmpeg.exe (Gyan release-essentials build) into crates/clipo/resources/
    so the next `tauri build` bundles it inside the installer.

.DESCRIPTION
    Bundled FFmpeg makes audio recording + GIF export work out of the box
    without a runtime download. The binary is gitignored — every dev /
    CI runs this script once before their first release build.

    Idempotent: if resources/ffmpeg.exe already exists, exits without
    re-downloading.
#>
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root 'crates\clipo\resources\ffmpeg.exe'

if (Test-Path $dest) {
    Write-Host "ffmpeg.exe already in place: $dest"
    exit 0
}

$url = 'https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip'
$zip = Join-Path $env:TEMP 'clipo-ffmpeg-fetch.zip'
$extract = Join-Path $env:TEMP 'clipo-ffmpeg-fetch'

Write-Host "Downloading $url ..."
$start = Get-Date
Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
$mb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host ("  -> {0} MB in {1:N1}s" -f $mb, ((Get-Date) - $start).TotalSeconds)

Write-Host "Extracting ffmpeg.exe ..."
if (Test-Path $extract) { Remove-Item -Recurse -Force $extract }
Expand-Archive -Path $zip -DestinationPath $extract -Force
$source = Get-ChildItem -Path $extract -Recurse -Filter 'ffmpeg.exe' | Select-Object -First 1
if (-not $source) {
    throw "ffmpeg.exe not found inside the downloaded zip"
}

New-Item -ItemType Directory -Force (Split-Path -Parent $dest) | Out-Null
Copy-Item -Path $source.FullName -Destination $dest -Force
Remove-Item -Recurse -Force $extract
Remove-Item -Force $zip

$destMb = [math]::Round((Get-Item $dest).Length / 1MB, 1)
Write-Host ("ffmpeg.exe placed at {0} ({1} MB)" -f $dest, $destMb)
