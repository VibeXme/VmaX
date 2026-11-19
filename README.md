# VmaX

Ultra-low-latency peer-to-peer remote desktop and game streaming.

VmaX connects a **host** (the machine being streamed) to a **client** over a
direct QUIC/UDP path when possible, automatically falling back to an
authenticated relay. Certificates are pinned by SHA-256 fingerprint exchanged
over the signaling channel.

## Features

- Low-latency QUIC transport with NAT traversal, relay fallback and path migration
- Hardware H.264/HEVC encoding (NVENC / Media Foundation) and D3D11 decode
- Opus audio with adaptive jitter buffering
- Remote keyboard, mouse and gamepad input
- Friend system, invites, presence and cloud accounts (Supabase)
- Windows desktop app and Android client

## Repository layout

| Path | Purpose |
| --- | --- |
| `src/` | React + TypeScript frontend |
| `src-tauri/` | Rust backend (Tauri v2) |
| `src-tauri/windows/` | NSIS installer script and prerequisites |
| `scripts` | Build helpers (`build_vmax_release.ps1`, `setup_android_env.ps1`) |
| `tools/` | Asset generation helpers |

## Building

### Windows

Requirements:

- Rust (stable, `x86_64-pc-windows-msvc`)
- Node.js 20+
- Visual Studio 2022 Build Tools with the **Desktop development with C++** workload
- Windows 10/11 SDK
- NSIS (for the installer)

```powershell
npm install
powershell -NoProfile -ExecutionPolicy Bypass -File .\build_vmax_release.ps1
```

Outputs (repository root):

- `VmaX.exe` — portable executable
- `VmaX-Setup.exe` — NSIS installer

### Android

Requirements:

- Android SDK (platform 34, build-tools 34+, NDK)
- JDK 17+
- Rust target `aarch64-linux-android`

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\setup_android_env.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\build_vmax_release.ps1 -SkipWindows -SkipInstaller
```

Output: `VmaX-Android-arm64.apk`

> On Windows without Developer Mode, Tauri cannot create the jniLibs symlink.
> The build script detects this and copies the compiled `libvmax_lib.so`
> into the Gradle project before packaging.

## Development

```powershell
npm install
npm run tauri dev
```

The Vite dev server listens on <http://localhost:1420>, matching
`devUrl` in `src-tauri/tauri.conf.json`.

## Tests

```powershell
cd src-tauri
cargo test --lib
```
