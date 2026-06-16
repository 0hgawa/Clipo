# Installer & auto-update

The in-app self-updater (Settings → "Check for updates") swaps **only the app
binary** — it downloads the new `clipo.exe`, verifies its minisign signature, and
replaces the running executable in place (per-user dir, no elevation). The NSIS
installer (`Clipo-Setup.exe`) is for the **first install** only; it ships
`clipo.exe` + the ffmpeg sidecar + shortcuts.

```
app polls  →  https://github.com/0hgawa/Clipo/releases/latest/download/latest.json
              { version, platforms.windows-x86_64.{ url, signature } }
                            │
              downloads clipo.exe, verifies `signature` against the public key
              baked into the app (src/settings.rs UPDATE_PUBKEY), swaps it in,
              relaunches (--updated) and exits. ffmpeg + shortcuts untouched.
```

Why only the binary: the installer is ~30 MB (mostly the 97 MB ffmpeg, which
never changes between versions). Shipping just `clipo.exe` makes each update
~4 MB instead of re-downloading the whole installer.

## Files here

| File | Tracked? | Purpose |
|------|----------|---------|
| `clipo.nsi` | ✅ | NSIS script — per-user install (`%LOCALAPPDATA%\Programs\Clipo`, no UAC), used for the first install. |
| `build.ps1` | ✅ | Builds installer → signs `clipo.exe` → writes `latest.json`. |
| `installer-header.bmp`, `installer-sidebar.bmp` | ✅ | Installer branding. |
| `Clipo-Setup.exe`, `latest.json` | ❌ (git-ignored) | Build outputs. |

No binaries are vendored: install the signing tool yourself (below).

## Prerequisites

- **makensis** — from your [NSIS](https://nsis.sourceforge.io) install; `build.ps1`
  finds it on `PATH` or under `Program Files`, or set `$env:MAKENSIS` to its path.
- **rsign2** on PATH — `cargo install rsign2`. The key in `.keys` is rsign-format,
  and rsign2 produces a minisign-compatible signature the app verifies (the same
  `minisign-verify` crate). Pure Rust, nothing vendored in the repo.
- **The minisign secret key** whose public half is embedded in the app
  (`src/settings.rs` `UPDATE_PUBKEY` = `RWSP1y9y…`, key `EA8DB70722FD78F`).
  `build.ps1 -Key` defaults to `D:\Apps\.keys\clipo.key`; signing prompts for the
  key password. If you sign with a *different* key, update `UPDATE_PUBKEY` and
  ship that build first, or the download is rejected.

## Cut a release

1. Bump the version in **two** places (they're independent):
   - `Cargo.toml` → `[package] version` (this gates the update: the feed version
     must be **greater** than what users run, or nothing is offered).
   - `installer/clipo.nsi` → `!define APP_VERSION` (+ `VIProductVersion`).
2. `cargo build --release` (from the repo root) — bakes the version/keys into
   `clipo.exe`.
3. From this folder:
   ```powershell
   powershell -ExecutionPolicy Bypass -File build.ps1
   ```
   Builds `Clipo-Setup.exe`, signs `clipo.exe`, and writes `latest.json`
   (prompts for the key password while signing).
4. Publish a GitHub release and upload **three** assets — the installer (first
   install), the signed binary (self-update target), and the feed:
   ```powershell
   gh release create v<version> `
     installer\Clipo-Setup.exe `
     target\release\clipo.exe `
     installer\latest.json `
     --title "Clipo <version>" --notes "Clipo <version>"
   ```
   It must be the latest (non-prerelease) release — that's the `releases/latest/`
   path the app polls.

> First release at `0.1.0` won't update anyone already on `0.1.0` — semver
> comparison only offers a *greater* version. Updates kick in from `0.1.0`.
