# VmaX — project notes for agents

## Release artifacts (standing instruction from the user)

After **every** change to the Rust or frontend sources, rebuild both release
artifacts so the copies in the repository root are always current:

```powershell
# Windows executable -> .\VmaX.exe
powershell -NoProfile -ExecutionPolicy Bypass -File .\build_vmax_release.ps1 -SkipAndroid

# Android APK -> .\VmaX-Android-arm64.apk
powershell -NoProfile -ExecutionPolicy Bypass -File .\build_vmax_release.ps1 -SkipAndroid:$false -SkipWindows
```

`build_vmax_release.ps1` with no switches builds both and failures are reported
per platform. The Windows build needs the Visual Studio C++ x64 tools; the
Android build needs the SDK/NDK, handled by `setup_android_env.ps1`.

Report the build outcome honestly: if a platform cannot be built on this
machine, say so and leave the previous artifact in place rather than claiming
it was refreshed.
