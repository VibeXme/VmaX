<#
.SYNOPSIS
    Generates Android launcher icons from the master artwork.
.DESCRIPTION
    Resizes src-tauri/icons/icon.png into the mipmap directories expected
    by the Android Gradle plugin, under the tauri-generated gen/android tree.
#>
param()

$ErrorActionPreference = 'Stop'

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$repoRoot = Resolve-Path (Join-Path $scriptDir '..')

$sourceIcon = Join-Path $repoRoot 'src-tauri\icons\icon.png'
if (-not (Test-Path $sourceIcon)) { throw "Source icon not found at $sourceIcon" }

$androidDir = Join-Path $repoRoot 'src-tauri\gen\android'
if (-not (Test-Path $androidDir)) { throw "Android project not found at $androidDir. Run 'tauri android init' first." }

$resDir = Join-Path $androidDir 'app\src\main\res'

# Density -> size in pixels (launcher icon)
$densities = @{
    'mipmap-mdpi'   = 48
    'mipmap-hdpi'   = 72
    'mipmap-xhdpi'  = 96
    'mipmap-xxhdpi' = 144
    'mipmap-xxxhdpi' = 192
}

Add-Type -AssemblyName System.Drawing
$source = [System.Drawing.Image]::FromFile($sourceIcon)

foreach ($density in $densities.Keys) {
    $size = $densities[$density]
    $targetDir = Join-Path $resDir $density
    New-Item -ItemType Directory -Force -Path $targetDir | Out-Null

    $target = Join-Path $targetDir 'ic_launcher.png'
    $bitmap = New-Object System.Drawing.Bitmap($size, $size)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $graphics.DrawImage($source, 0, 0, $size, $size)
    $graphics.Dispose()
    $bitmap.Save($target, [System.Drawing.Imaging.ImageFormat]::Png)
    $bitmap.Dispose()
    Write-Host "Generated $target ($size x $size)"
}

$source.Dispose()
Write-Host "Android launcher icons generated."
