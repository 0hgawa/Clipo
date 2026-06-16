# Build the release, sign clipo.exe, and emit latest.json for the in-app
# self-updater; also build Clipo-Setup.exe for first-time installs.
#
#   powershell -ExecutionPolicy Bypass -File build.ps1
#
# The self-updater downloads and swaps ONLY clipo.exe (verified against the
# minisign key embedded in the app, src/settings.rs UPDATE_PUBKEY). The NSIS
# installer is just for the first install (ships clipo.exe + ffmpeg + shortcuts).
# Signing uses rsign2 (`cargo install rsign2`) - the key in .keys is rsign-format
# and rsign2 produces a minisign-compatible signature. It prompts for the key
# password.
#   -Key   path to the rsign/minisign secret key (default: D:\Apps\.keys\clipo.key)
#   -Repo  GitHub owner/repo the release is published to (for the asset URL)
param(
    [string]$Key  = "D:\Apps\.keys\clipo.key",
    [string]$Repo = "0hgawa/Clipo"
)
$ErrorActionPreference = "Stop"

$exe = "D:\Apps\Clipo\target\release\clipo.exe"
if (-not (Test-Path $exe)) {
    throw "clipo.exe not found. Run 'cargo build --release' in 'D:\Apps\Clipo' first."
}

# 1. First-install installer (NSIS). Found on PATH or a standard NSIS install;
#    set $env:MAKENSIS to point at makensis.exe explicitly.
$makensis = if ($env:MAKENSIS) { $env:MAKENSIS } else {
    @(
        (Get-Command makensis -ErrorAction SilentlyContinue).Source,
        (Join-Path ${env:ProgramFiles(x86)} "NSIS\makensis.exe"),
        (Join-Path $env:ProgramFiles "NSIS\makensis.exe")
    ) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
}
if (-not $makensis) {
    throw "makensis not found. Install NSIS (https://nsis.sourceforge.io) or set `$env:MAKENSIS to its path."
}
$nsi   = Join-Path $PSScriptRoot "clipo.nsi"
$setup = Join-Path $PSScriptRoot "Clipo-Setup.exe"
& $makensis $nsi
if ($LASTEXITCODE -ne 0) { throw "makensis failed ($LASTEXITCODE)" }
if (-not (Test-Path $setup)) { throw "Clipo-Setup.exe was not produced." }

# 2. Sign clipo.exe (rsign2) - this is what the self-updater verifies
$rsign = (Get-Command rsign -ErrorAction SilentlyContinue).Source
if (-not $rsign) {
    throw "rsign not found on PATH. Install it: 'cargo install rsign2'. (The key in .keys is rsign-format; rsign2 produces a minisign-compatible signature the app verifies.)"
}
if (-not (Test-Path $Key)) { throw "secret key not found at $Key (override with -Key)." }

$sig = "$exe.minisig"
if (Test-Path $sig) { Remove-Item $sig -Force }
& $rsign sign -s $Key -x $sig $exe          # prompts for the key password
if ($LASTEXITCODE -ne 0) { throw "rsign signing failed ($LASTEXITCODE)" }
# .NET read: Get-Content -Raw attaches PSPath note-properties that ConvertTo-Json
# would serialize as an object instead of the raw .minisig string.
$signature = [System.IO.File]::ReadAllText($sig)

# 3. Emit latest.json
# Read the app's [package] version from Cargo.toml (not the workspace one).
$cargoLines = Get-Content "D:\Apps\Clipo\Cargo.toml"
$pkgLine = ($cargoLines | Select-String -Pattern '^\[package\]' | Select-Object -First 1).LineNumber
$version = ($cargoLines[$pkgLine..($cargoLines.Count - 1)] |
    Select-String -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches.Groups[1].Value
if (-not $version) { throw "couldn't read [package] version from Cargo.toml" }

$manifest = [ordered]@{
    version   = $version
    pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    platforms = [ordered]@{
        "windows-x86_64" = [ordered]@{
            # GitHub serves the newest release's asset from this stable path.
            url       = "https://github.com/$Repo/releases/latest/download/clipo.exe"
            signature = $signature
        }
    }
}
$json = $manifest | ConvertTo-Json -Depth 6
$latest = Join-Path $PSScriptRoot "latest.json"
# UTF-8 without BOM: PS 5.1's `-Encoding utf8` adds a BOM and serde_json
# (the app's feed parser) rejects it.
[System.IO.File]::WriteAllText($latest, $json, (New-Object System.Text.UTF8Encoding $false))

Write-Host ""
Write-Host "Done:"
Write-Host "  installer (first install) -> $setup"
Write-Host "  app binary (self-update)  -> $exe"
Write-Host "  signature                 -> $sig"
Write-Host "  manifest                  -> $latest  (v$version)"
Write-Host ""
Write-Host "Upload Clipo-Setup.exe, clipo.exe and latest.json as assets on the GitHub release."
