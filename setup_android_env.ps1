<#
.SYNOPSIS
    Configures the Android SDK and JDK environment for the VmaX Android build.
.DESCRIPTION
    Discovers the Android SDK and a JDK (preferring the toolchains bundled with
    Android Studio) and exports ANDROID_HOME, ANDROID_SDK_ROOT, JAVA_HOME and
    appends the relevant tool directories to PATH for the current process.
    Call this before any Android build step. It warns (rather than fails) when a
    toolchain cannot be located so the caller can decide how to proceed.
#>
param()

function Find-AndroidSdk {
    $candidates = @(
        $env:ANDROID_HOME,
        $env:ANDROID_SDK_ROOT,
        "$env:LOCALAPPDATA\Android\Sdk",
        'C:\Program Files\Android\Sdk',
        'C:\Program Files\Android\Android Studio\sdk',
        "$env:LOCALAPPDATA\Programs\Android\Android Studio\sdk",
        'C:\Android\sdk',
        'D:\Android\sdk'
    ) | Where-Object { $_ -and $_ -ne '' }
    foreach ($c in $candidates) {
        if (Test-Path (Join-Path $c 'platform-tools\adb.exe')) {
            return $c
        }
    }
    return $null
}

function Find-JavaHome {
    if ($env:JAVA_HOME -and (Test-Path (Join-Path $env:JAVA_HOME 'bin\java.exe'))) {
        return $env:JAVA_HOME
    }
    $roots = @(
        'C:\Program Files\Android\Android Studio\jbr',
        'C:\Program Files\Android\Android Studio\jre',
        'C:\Program Files\Java',
        'C:\Program Files (x86)\Java',
        "$env:LOCALAPPDATA\Programs\Android\Android Studio\jbr"
    )
    foreach ($root in $roots) {
        if (-not (Test-Path $root)) { continue }
        if (Test-Path (Join-Path $root 'bin\java.exe')) { return $root }
        $dirs = @(Get-ChildItem -LiteralPath $root -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match '^jdk' } | Sort-Object Name -Descending)
        foreach ($d in $dirs) {
            if (Test-Path (Join-Path $d.FullName 'bin\java.exe')) { return $d.FullName }
        }
    }
    return $null
}

$sdk = Find-AndroidSdk
if ($sdk) {
    $env:ANDROID_HOME = $sdk
    $env:ANDROID_SDK_ROOT = $sdk
    Write-Host "Using Android SDK: $sdk"

    $pathAdds = New-Object System.Collections.Generic.List[string]
    $pathAdds.Add((Join-Path $sdk 'platform-tools'))
    $cmdToolsLatest = Join-Path (Join-Path $sdk 'cmdline-tools') 'latest'
    $cmdToolsLatest = Join-Path $cmdToolsLatest 'bin'
    if (Test-Path $cmdToolsLatest) { $pathAdds.Add($cmdToolsLatest) }
    $toolsBin = Join-Path (Join-Path $sdk 'tools') 'bin'
    if (Test-Path $toolsBin) { $pathAdds.Add($toolsBin) }
    $buildToolsDir = Join-Path $sdk 'build-tools'
    if (Test-Path $buildToolsDir) {
        $latest = @(Get-ChildItem -LiteralPath $buildToolsDir -Directory -ErrorAction SilentlyContinue |
            Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1)
        if ($latest) { $pathAdds.Add($latest.FullName) }
    }
    foreach ($p in $pathAdds) {
        if (Test-Path $p) { $env:PATH = "$p;$env:PATH" }
    }
} else {
    Write-Warning 'Android SDK not found. Set ANDROID_HOME to the SDK location before building Android.'
}

$jdk = Find-JavaHome
if ($jdk) {
    $env:JAVA_HOME = $jdk
    $env:PATH = "$(Join-Path $jdk 'bin');$env:PATH"
    Write-Host "Using JDK: $jdk"
} else {
    Write-Warning 'JDK not found. Set JAVA_HOME to a JDK location before building Android.'
}
