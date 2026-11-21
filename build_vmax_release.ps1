<#
.SYNOPSIS
    Builds the x64 Windows executable, NSIS installer, and ARM64 Android APK at the workspace root.
.DESCRIPTION
    Run from PowerShell with Visual Studio C++ Build Tools, NSIS, and the Android SDK
    available. Use -SkipWindows or -SkipAndroid to build a single platform.
    If Gradle produces an unsigned APK, it is signed with a local development key.
#>
param(
    [switch]$SkipWindows,
    [switch]$SkipAndroid,
    [switch]$SkipInstaller
)

$ErrorActionPreference = 'Stop'
$toolsDir = Join-Path $env:LOCALAPPDATA 'VmaXBuildTools'
$errors = [System.Collections.Generic.List[string]]::new()
$script:WindowsBuildSucceeded = $false

# Prerequisite download URLs (version-pinned for reproducibility)
$VC_REDIST_X64_URL = 'https://aka.ms/vs/17/release/vc_redist.x64.exe'
# NSIS is installed via winget (NSIS.NSIS) - no download needed

function Invoke-Tauri([string[]]$Arguments) {
    & npm.cmd run tauri -- @Arguments
    if ($LASTEXITCODE) { throw "Tauri $($Arguments[0]) failed with exit code $LASTEXITCODE." }
}

function Stop-RunningVmax {
    $processes = Get-Process -Name 'VmaX' -ErrorAction SilentlyContinue
    if ($processes) {
        Write-Host 'Stopping running VmaX.exe instances to release file locks...'
        $processes | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
        $processes | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
        $still = Get-Process -Name 'VmaX' -ErrorAction SilentlyContinue
        if ($still) { throw 'Could not stop running VmaX.exe instances; close them manually and retry.' }
    }
}

function Remove-LockedDirectory([string]$Path, [int]$Retries = 5) {
    if (-not (Test-Path $Path)) { return }
    $lastError = $null
    for ($i = 0; $i -lt $Retries; $i++) {
        try {
            Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction Stop
            return
        } catch [System.IO.IOException] {
            $lastError = $_
            Start-Sleep -Seconds 2
        }
    }
    throw "Could not remove locked directory '$Path': $lastError"
}

function Download-Prerequisites {
    $prereqDir = Join-Path $PSScriptRoot 'src-tauri\windows\nsis'
    New-Item -ItemType Directory -Force -Path $prereqDir | Out-Null

    # VC++ Redistributable 2015-2022 x64 (for installer)
    $vcRedistPath = Join-Path $prereqDir 'vc_redist.x64.exe'
    if (-not (Test-Path $vcRedistPath)) {
        Write-Host "Downloading VC++ Redistributable..."
        try {
            Invoke-WebRequest -Uri $VC_REDIST_X64_URL -OutFile $vcRedistPath -UseBasicParsing
            if ($LASTEXITCODE) { throw "VC++ Redist download failed with exit code $LASTEXITCODE." }
        } catch { throw "Failed to download VC++ Redistributable: $_" }
    }

    # NSIS (installed via winget - NSIS.NSIS)
    $makensisPath = "C:\Program Files (x86)\NSIS\Bin\makensis.exe"
    if (-not (Test-Path $makensisPath)) {
        $makensisPath = "C:\Program Files\NSIS\Bin\makensis.exe"
    }
    if (-not (Test-Path $makensisPath)) {
        throw "NSIS not found. Install with: winget install --id NSIS.NSIS --source winget"
    }
    $script:MAKE_NSIS = $makensisPath
}

function Initialize-WindowsBuild {
    if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        $devShell = if (Test-Path $vswhere) {
            & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find 'Common7\Tools\VsDevCmd.bat'
        }
        if (-not $devShell -and (Test-Path $vswhere)) {
            $devShell = & $vswhere -latest -products '*' -find 'Common7\Tools\VsDevCmd.bat'
        }
        if (-not $devShell) { throw 'Install the Visual Studio C++ x64 build tools.' }
        $variables = & cmd.exe /d /c ('call "' + $devShell + '" -no_logo -arch=x64 -host_arch=x64 && set')
        if ($LASTEXITCODE) { throw 'Visual Studio developer environment initialization failed.' }
        foreach ($line in $variables) {
            if ($line -match '^([^=]+)=(.*)$') {
                [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], 'Process')
            }
        }
    }

    # Support a Windows SDK extracted from the official Microsoft NuGet packages.
    $sdk = Join-Path $toolsDir 'microsoft.windows.sdk.cpp\c'
    $sdkLib = Join-Path $toolsDir 'microsoft.windows.sdk.cpp.x64\c'
    if ((Test-Path "$sdkLib\um\x64\kernel32.lib") -and (Test-Path "$sdk\Include")) {
        $headers = Get-ChildItem "$sdk\Include" -Directory |
            Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
        $include = $headers.FullName
        $env:INCLUDE = "$include\ucrt;$include\shared;$include\um;$include\winrt;$env:INCLUDE"
        $env:LIB = "$sdkLib\ucrt\x64;$sdkLib\um\x64;$env:LIB"
        $env:PATH = "$sdk\bin\$($headers.Name)\x64;$env:PATH"
    }
}

function Get-CargoTargetDirectory {
    $metadata = & cargo metadata --manifest-path src-tauri/Cargo.toml --format-version 1 --no-deps --locked |
        ConvertFrom-Json
    if ($LASTEXITCODE) { throw 'Could not resolve the Cargo target directory.' }
    return $metadata.target_directory
}

function Build-Windows {
    Stop-RunningVmax
    $target = 'x86_64-pc-windows-msvc'
    Invoke-Tauri @('build', '--ci', '--no-bundle', '--target', $target, '--', '--locked')
    $exe = Join-Path (Get-CargoTargetDirectory) "$target\release\vmax.exe"
    $imports = & dumpbin.exe /nologo /dependents $exe
    if ($LASTEXITCODE) { throw 'Could not verify the Windows executable dependencies.' }
    if ($imports -match '^\s*(WebView2Loader|VCRUNTIME\w*|MSVCP\w*)\.dll\s*$') {
        throw 'The Windows executable still requires a companion loader or Visual C++ runtime DLL.'
    }
    $destination = Join-Path $PSScriptRoot 'VmaX.exe'
    try {
        Copy-Item -LiteralPath $exe -Destination $destination -Force
    } catch [System.IO.IOException] {
        # A running Windows image can be renamed even when its bytes are locked.
        $previous = Join-Path (Get-CargoTargetDirectory) ('VmaX-previous-' + [guid]::NewGuid().ToString('N') + '.exe')
        Move-Item -LiteralPath $destination -Destination $previous
        try {
            Copy-Item -LiteralPath $exe -Destination $destination
        } catch {
            Move-Item -LiteralPath $previous -Destination $destination -Force
            throw
        }
    }
    Write-Host "Windows executable at $PSScriptRoot\VmaX.exe"
    $script:WindowsBuildSucceeded = $true
}

function Build-WindowsInstaller {
    if ($SkipInstaller) { return }

    # Stage app files for NSIS
    $target = 'x86_64-pc-windows-msvc'
    $exe = Join-Path (Get-CargoTargetDirectory) "$target\release\vmax.exe"
    if (-not (Test-Path $exe)) { throw "Windows executable not found at $exe. Run Build-Windows first." }

    $stagingDir = Join-Path $PSScriptRoot 'src-tauri\windows\nsis\app'
    Remove-LockedDirectory $stagingDir
    New-Item -ItemType Directory -Force -Path $stagingDir | Out-Null

    # Copy the executable and any companion files (none expected, but be safe)
    Copy-Item -LiteralPath $exe -Destination (Join-Path $stagingDir 'VmaX.exe') -Force

    # Also copy WebView2/VC++ bootstrappers to staging (NSIS embeds them)
    $prereqDir = Join-Path $PSScriptRoot 'src-tauri\windows\nsis'
    Copy-Item -LiteralPath (Join-Path $prereqDir 'MicrosoftEdgeWebview2Setup.exe') -Destination $stagingDir -Force
    Copy-Item -LiteralPath (Join-Path $prereqDir 'vc_redist.x64.exe') -Destination $stagingDir -Force

    # Run NSIS
    $nsiScript = Join-Path $PSScriptRoot 'src-tauri\windows\nsis\vmax.nsi'
    Write-Host "Building NSIS installer..."
    & $script:MAKE_NSIS $nsiScript
    if ($LASTEXITCODE) { throw "NSIS build failed with exit code $LASTEXITCODE." }

    # Move installer to repo root
    $installer = Join-Path $PSScriptRoot 'src-tauri\windows\nsis\VmaX-Setup.exe'
    if (-not (Test-Path $installer)) { throw "NSIS installer not produced at $installer." }
    $destination = Join-Path $PSScriptRoot 'VmaX-Setup.exe'
    Copy-Item -LiteralPath $installer -Destination $destination -Force
    Write-Host "Windows installer at $destination"

    # Cleanup staging
    Remove-LockedDirectory $stagingDir
}

function Build-Android {
    # On Windows PowerShell 5.1, a Stop ErrorActionPreference turns every native
    # stderr line redirected through 2>&1 (npm/tauri/gradle progress output)
    # into a terminating error. Relax it for the duration of this function;
    # real failures are still caught via $LASTEXITCODE and throw below.
    $ErrorActionPreference = 'Continue'
    & (Join-Path $PSScriptRoot 'setup_android_env.ps1')
    $androidDir = Join-Path $PSScriptRoot 'src-tauri\gen\android'
    if (-not (Test-Path (Join-Path $androidDir 'gradlew.bat'))) {
        Invoke-Tauri @('android', 'init', '--ci')
    }

    # gen/android is gitignored and recreated by `tauri android init`, so the
    # launcher icons cannot live in source control: they are derived from the
    # master artwork here, after the tree exists and before Gradle packages it.
    & (Join-Path $PSScriptRoot 'tools\make_android_icons.ps1')
    if ($LASTEXITCODE) {
        throw "Android launcher icon generation failed with exit code $LASTEXITCODE."
    }

    $log = New-TemporaryFile
    try {
        & npm.cmd run tauri -- android build --ci --apk --target aarch64 -- --locked 2>&1 |
            Tee-Object -FilePath $log.FullName | Out-Host
        if ($LASTEXITCODE) {
            if (-not (Select-String -LiteralPath $log.FullName -SimpleMatch 'Creation symbolic link is not allowed' -Quiet)) {
                throw "Tauri Android build failed with exit code $LASTEXITCODE."
            }
            # Tauri finished compiling Rust and generating the Gradle project.
            # Stage its library by copying when Windows lacks symlink privileges.
            $library = Join-Path (Get-CargoTargetDirectory) 'aarch64-linux-android\release\libvmax_lib.so'
            $jniDir = Join-Path $androidDir 'app\src\main\jniLibs\arm64-v8a'
            New-Item -ItemType Directory -Force -Path $jniDir | Out-Null
            Copy-Item -LiteralPath $library -Destination $jniDir -Force
            $assets = Join-Path $androidDir 'app\src\main\assets'
            New-Item -ItemType Directory -Force -Path $assets | Out-Null
            $config = Get-Content src-tauri/tauri.conf.json -Raw | ConvertFrom-Json
            if (-not $config.PSObject.Properties['plugins']) {
                $config | Add-Member -MemberType NoteProperty -Name plugins -Value ([pscustomobject]@{})
            }
            $config | ConvertTo-Json -Depth 100 |
                Set-Content -LiteralPath (Join-Path $assets 'tauri.conf.json') -Encoding utf8
            Push-Location $androidDir
            try {
                & .\gradlew.bat :app:assembleUniversalRelease '-PabiList=arm64-v8a' '-ParchList=arm64' '-PtargetList=aarch64' -x :app:rustBuildArm64Release --console=plain
                if ($LASTEXITCODE) { throw "Gradle APK packaging failed with exit code $LASTEXITCODE." }
            } finally {
                Pop-Location
            }
        }
    } finally {
        Remove-Item -LiteralPath $log.FullName
    }
    $apkDir = Join-Path $androidDir 'app\build\outputs\apk\universal\release'
    $apk = Get-ChildItem $apkDir -Filter '*.apk' -File |
        Where-Object Name -ne 'vmax-local-signed.apk' |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $apk) { throw "No Android release APK was produced in $apkDir." }
    $buildTools = Get-ChildItem (Join-Path $env:ANDROID_HOME 'build-tools') -Directory |
        Where-Object { Test-Path (Join-Path $_.FullName 'apksigner.bat') } |
        Sort-Object { [version]($_.Name -split '-')[0] } -Descending | Select-Object -First 1
    if (-not $buildTools) { throw 'Android SDK Build-Tools with apksigner are required.' }

    $apksigner = Join-Path $buildTools.FullName 'apksigner.bat'
    if ($env:VMAX_ANDROID_KEYSTORE_BASE64) {
        if (-not ($env:VMAX_ANDROID_KEYSTORE_PASSWORD -and $env:VMAX_ANDROID_KEY_ALIAS -and $env:VMAX_ANDROID_KEY_PASSWORD)) {
            throw 'Set the Android release keystore password, key alias, and key password.'
        }
        $keyFile = New-TemporaryFile
        try {
            [IO.File]::WriteAllBytes($keyFile.FullName, [Convert]::FromBase64String($env:VMAX_ANDROID_KEYSTORE_BASE64))
            $signed = Join-Path $apkDir 'vmax-local-signed.apk'
            & $apksigner sign --ks $keyFile.FullName --ks-key-alias $env:VMAX_ANDROID_KEY_ALIAS --ks-pass env:VMAX_ANDROID_KEYSTORE_PASSWORD --key-pass env:VMAX_ANDROID_KEY_PASSWORD --out $signed $apk.FullName
            if ($LASTEXITCODE) { throw 'APK release signing failed.' }
        } finally {
            Remove-Item -LiteralPath $keyFile.FullName -Force
        }
    } elseif ($apk.Name -like '*-unsigned.apk') {
        $keyDir = Join-Path $env:LOCALAPPDATA 'VmaX\android-build'
        New-Item -ItemType Directory -Force -Path $keyDir | Out-Null
        $keystore = Join-Path $keyDir 'debug.keystore'
        if (-not (Test-Path $keystore)) {
            & "$env:JAVA_HOME\bin\keytool.exe" -genkeypair -keystore $keystore -storepass android -keypass android -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 -dname 'CN=VmaX Android Development'
            if ($LASTEXITCODE) { throw 'Could not generate the local Android development key.' }
        }
        $signed = Join-Path $apkDir 'vmax-local-signed.apk'
        & $apksigner sign --ks $keystore --ks-key-alias androiddebugkey --ks-pass pass:android --key-pass pass:android --out $signed $apk.FullName
        if ($LASTEXITCODE) { throw 'APK signing failed.' }
    } else {
        $signed = $apk.FullName
    }
    & $apksigner verify $signed
    if ($LASTEXITCODE) { throw 'APK signature verification failed.' }
    $aapt = Join-Path $buildTools.FullName 'aapt.exe'
    $permissions = & $aapt dump permissions $signed
    if ($LASTEXITCODE -or -not ($permissions -match "android\.permission\.CAMERA")) {
        throw 'Android APK is missing the camera permission required by the invite QR scanner.'
    }
    $dexdump = Join-Path $buildTools.FullName 'dexdump.exe'
    & node (Join-Path $PSScriptRoot 'tools\verify-android-decoder.mjs') $signed $dexdump
    if ($LASTEXITCODE) { throw 'Android APK decoder JNI verification failed.' }
    Copy-Item -LiteralPath $signed -Destination (Join-Path $PSScriptRoot 'VmaX-Android-arm64.apk') -Force
    Write-Host "Android APK at $PSScriptRoot\VmaX-Android-arm64.apk"
}

Push-Location $PSScriptRoot
try {
    if (-not (Test-Path 'node_modules\.bin\tauri.cmd')) {
        & npm.cmd ci
        if ($LASTEXITCODE) { throw 'npm ci failed.' }
    }
    if (-not ($SkipWindows -and $SkipAndroid)) {
        # Android builds also link host-side Rust build scripts on Windows.
        if ($SkipWindows -and $env:RUSTUP_TOOLCHAIN -like '*-windows-gnu' -and (Get-Command gcc.exe -ErrorAction SilentlyContinue)) {
            # Android-only build on a machine without Visual Studio: host-side
            # build scripts link with the GNU toolchain instead, so the caller
            # must set RUSTUP_TOOLCHAIN=stable-x86_64-pc-windows-gnu and put
            # the mingw-w64 gcc on PATH.
            Write-Host 'Using the selected GNU toolchain for host-side build scripts.'
        } else {
            Initialize-WindowsBuild
        }
    }

    # Download prerequisites early (needed for Windows + installer)
    if (-not $SkipWindows) {
        Download-Prerequisites
    }

    if (-not $SkipWindows) {
        try { Build-Windows } catch { $errors.Add("Windows build: $_") }
        # Installer needs the Windows exe, so build it only after a successful Windows build.
        if (-not $SkipInstaller) {
            if ($script:WindowsBuildSucceeded) {
                try { Build-WindowsInstaller } catch { $errors.Add("Windows installer: $_") }
            } else {
                Write-Host 'Skipping Windows installer because the Windows build did not succeed.'
            }
        }
    }
    if (-not $SkipAndroid) {
        try { Build-Android } catch { $errors.Add("Android build: $_") }
    }
    if ($errors.Count) { throw ($errors -join [Environment]::NewLine) }
} finally {
    Pop-Location
}
