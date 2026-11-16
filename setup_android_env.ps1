<#
.SYNOPSIS
    Finds the Android JDK, SDK and NDK for VmaX and installs the Rust ARM64 target.
.DESCRIPTION
    Honors JAVA_HOME, ANDROID_HOME and NDK_HOME. Uses the standard Android SDK
    path or a local VmaXBuildTools directory when those variables are unset.
#>
$ErrorActionPreference = 'Stop'
$toolsDir = Join-Path $env:LOCALAPPDATA 'VmaXBuildTools'

if (-not $env:JAVA_HOME) {
    $jdk = Get-ChildItem $toolsDir -Directory -Filter 'jdk-*' -ErrorAction SilentlyContinue |
        Where-Object { Test-Path (Join-Path $_.FullName 'bin\java.exe') } |
        Sort-Object Name -Descending | Select-Object -First 1
    $env:JAVA_HOME = if ($jdk) { $jdk.FullName } else { 'C:\Program Files\Android\Android Studio\jbr' }
}
if (-not (Test-Path (Join-Path $env:JAVA_HOME 'bin\java.exe'))) {
    throw 'Set JAVA_HOME to an installed JDK.'
}

if (-not $env:ANDROID_HOME) {
    $env:ANDROID_HOME = $env:ANDROID_SDK_ROOT
}
if (-not $env:ANDROID_HOME) {
    $env:ANDROID_HOME = @(
        (Join-Path $env:LOCALAPPDATA 'Android\Sdk'),
        (Join-Path $toolsDir 'android-sdk')
    ) | Where-Object { Test-Path (Join-Path $_ 'platforms') } | Select-Object -First 1
}
if (-not $env:ANDROID_HOME -or -not (Test-Path $env:ANDROID_HOME)) {
    throw 'Set ANDROID_HOME to an installed Android SDK.'
}

if (-not $env:NDK_HOME) {
    $env:NDK_HOME = $env:ANDROID_NDK_HOME
}
if (-not $env:NDK_HOME) {
    $ndk = Get-ChildItem (Join-Path $env:ANDROID_HOME 'ndk') -Directory -ErrorAction SilentlyContinue |
        Where-Object { Test-Path (Join-Path $_.FullName 'source.properties') } |
        Sort-Object { [version]($_.Name -split '-')[0] } -Descending | Select-Object -First 1
    if ($ndk) { $env:NDK_HOME = $ndk.FullName }
}
if (-not $env:NDK_HOME -or -not (Test-Path (Join-Path $env:NDK_HOME 'source.properties'))) {
    throw 'Install the Android NDK and set NDK_HOME to its directory.'
}
$env:ANDROID_NDK_HOME = $env:NDK_HOME
$env:PATH = "$env:JAVA_HOME\bin;$env:ANDROID_HOME\platform-tools;$env:PATH"

$target = 'aarch64-linux-android'
if ((& rustup target list --installed) -notcontains $target) {
    & rustup target add $target
    if ($LASTEXITCODE) { throw "Could not install the Rust target $target." }
}
