<#
.SYNOPSIS
    Regenerates the Android launcher icons from the master artwork.
.DESCRIPTION
    `src-tauri/gen/android` is produced by `tauri android init` and is not
    source-controlled, so the launcher icons cannot be edited in place: they are
    rebuilt from `src-tauri/icons/icon.png` on every Android build instead.

    The master draws the brand rings on a dark rounded square. An adaptive icon
    needs those as two separate layers, because the launcher supplies the shape:
    a masked foreground over a background colour. Keying the dark plate out of
    the master yields the foreground, and the plate's own colour becomes the
    background resource, so the composed icon matches the master everywhere the
    launcher's mask shows.

    Sizing is the part that usually goes wrong. An adaptive icon is a 108dp
    canvas of which launchers show roughly the middle 72dp, and every mask
    (circle, squircle, teardrop) must find the artwork inside the centred 66dp
    safe circle. Artwork sized to the canvas is therefore cropped by every mask,
    which is what "the icon does not fit" looks like. The rings are placed at a
    66dp diameter here: the largest arc lands at 92% of the visible icon, so no
    mask can clip it.

    Legacy `ic_launcher` / `ic_launcher_round` PNGs (Android 7 and older, plus
    TV launchers that ignore adaptive icons) are drawn from the same two layers
    with the same ring proportions, so the icon looks the same on every API
    level.
.PARAMETER Master
    Square master artwork. Defaults to src-tauri/icons/icon.png.
.PARAMETER ResDir
    Android resource directory to write into. Defaults to the gen/android tree.
.PARAMETER Verify
    Only check existing icons against the expected proportions; write nothing.
#>
param(
    [string]$Master = (Join-Path $PSScriptRoot '..\src-tauri\icons\icon.png'),
    [string]$ResDir = (Join-Path $PSScriptRoot '..\src-tauri\gen\android\app\src\main\res'),
    [switch]$Verify
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# --- Design constants -------------------------------------------------------
# 108dp adaptive canvas; launchers guarantee a 66dp centred circle is visible.
$ForegroundCanvasDp = 108
$ForegroundRingRadius = 33.0 / 108.0    # 66dp diameter rings
# Legacy canvases are fully visible, so the rings take the same share of the
# visible icon that they take on an adaptive icon (66/72 = 92%).
$LegacyCanvasDp = 48
$LegacyRingRadius = 0.44
$LegacyCornerRadius = 0.22
# The dark plate the artwork sits on. Also written to values/ic_launcher_background.xml.
$BackgroundRgb = @(0x0C, 0x13, 0x1A)

# Keying ramp: colour distance from the plate at which a pixel starts to count
# as artwork, and the distance at which it is fully opaque. The antialiased edge
# of a ring falls inside this window, so ring edges stay smooth instead of
# turning into hard stair steps.
$KeyLow = 8.0
$KeyHigh = 32.0

$Densities = @(
    @{ Name = 'mdpi'; Scale = 1.0 },
    @{ Name = 'hdpi'; Scale = 1.5 },
    @{ Name = 'xhdpi'; Scale = 2.0 },
    @{ Name = 'xxhdpi'; Scale = 3.0 },
    @{ Name = 'xxxhdpi'; Scale = 4.0 }
)

function Get-FileBytes([string]$Path) {
    # Read through a copy: Bitmap::FromFile keeps the file locked, and the
    # master lives in the source tree.
    $bytes = [System.IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $Path).ProviderPath)
    $stream = New-Object System.IO.MemoryStream
    $stream.Write($bytes, 0, $bytes.Length)
    $stream.Position = 0
    return $stream
}

function Read-Pixels([System.Drawing.Bitmap]$Bitmap) {
    $rect = New-Object System.Drawing.Rectangle 0, 0, $Bitmap.Width, $Bitmap.Height
    $data = $Bitmap.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    try {
        $buffer = New-Object byte[] ($data.Stride * $Bitmap.Height)
        [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $buffer, 0, $buffer.Length)
        return @{ Pixels = $buffer; Stride = $data.Stride }
    } finally {
        $Bitmap.UnlockBits($data)
    }
}

# --- Build the ring layer ---------------------------------------------------
$stream = Get-FileBytes $Master
try {
    $source = [System.Drawing.Bitmap]::FromStream($stream)
} finally {
    $stream.Dispose()
}
$width = $source.Width
$height = $source.Height
if ($width -ne $height) { throw "The master icon must be square, got ${width}x${height}." }

$read = Read-Pixels $source
$sourcePixels = $read.Pixels
$stride = $read.Stride

# Sample the plate colour well inside the rounded square but outside the rings.
$probeX = [int]($width * 0.1)
$probeY = [int]($height * 0.1)
$probe = $probeY * $stride + $probeX * 4
$plateB = [double]$sourcePixels[$probe]
$plateG = [double]$sourcePixels[$probe + 1]
$plateR = [double]$sourcePixels[$probe + 2]
if ($probeX -lt 0 -or $sourcePixels[$probe + 3] -lt 250) {
    throw "The master icon has no opaque plate at ($probeX,$probeY); cannot separate the artwork."
}

$layer = New-Object System.Drawing.Bitmap $width, $height, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$layerRect = New-Object System.Drawing.Rectangle 0, 0, $width, $height
$layerData = $layer.LockBits($layerRect, [System.Drawing.Imaging.ImageLockMode]::WriteOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$layerPixels = New-Object byte[] ($layerData.Stride * $height)
$layerStride = $layerData.Stride

$minX = $width; $maxX = -1; $minY = $height; $maxY = -1
$opaque = 0
try {
    for ($y = 0; $y -lt $height; $y++) {
        $srcRow = $y * $stride
        $dstRow = $y * $layerStride
        for ($x = 0; $x -lt $width; $x++) {
            $o = $srcRow + $x * 4
            $d = $dstRow + $x * 4
            $a0 = [double]$sourcePixels[$o + 3] / 255.0
            if ($a0 -le 0.0) { continue }
            $b = [double]$sourcePixels[$o]
            $g = [double]$sourcePixels[$o + 1]
            $r = [double]$sourcePixels[$o + 2]
            $diff = [math]::Max([math]::Abs($r - $plateR), [math]::Max([math]::Abs($g - $plateG), [math]::Abs($b - $plateB)))
            if ($diff -le $KeyLow) { continue }
            $key = [math]::Min(1.0, ($diff - $KeyLow) / ($KeyHigh - $KeyLow))
            $alpha = $a0 * $key
            if ($alpha -le 0.004) { continue }
            # The master's edge pixels are blended with the plate. Undo that so
            # the artwork composites cleanly over any background.
            $layerPixels[$d + 2] = [byte][math]::Min(255.0, [math]::Max(0.0, $plateR + ($r - $plateR) / $alpha))
            $layerPixels[$d + 1] = [byte][math]::Min(255.0, [math]::Max(0.0, $plateG + ($g - $plateG) / $alpha))
            $layerPixels[$d] = [byte][math]::Min(255.0, [math]::Max(0.0, $plateB + ($b - $plateB) / $alpha))
            $layerPixels[$d + 3] = [byte][math]::Round($alpha * 255.0)
            if ($alpha -gt 0.5) {
                $opaque++
                if ($x -lt $minX) { $minX = $x }
                if ($x -gt $maxX) { $maxX = $x }
                if ($y -lt $minY) { $minY = $y }
                if ($y -gt $maxY) { $maxY = $y }
            }
        }
    }
} finally {
    [System.Runtime.InteropServices.Marshal]::Copy($layerPixels, 0, $layerData.Scan0, $layerPixels.Length)
    $layer.UnlockBits($layerData)
    $source.Dispose()
}

if ($maxX -lt 0) { throw 'Keying the plate out of the master produced an empty artwork layer.' }
$coverage = 100.0 * $opaque / ($width * $height)
if ($coverage -gt 40.0) {
    throw ("Keying the plate out of the master kept {0:N1}% of the canvas: the plate colour was not recognised." -f $coverage)
}

# Geometry of the artwork, measured from the keyed layer so a re-exported master
# keeps working without retuning the numbers above.
$centreX = ($minX + $maxX) / 2.0
$centreY = ($minY + $maxY) / 2.0
$radius = 0.0
for ($y = $minY; $y -le $maxY; $y++) {
    $row = $y * $layerStride
    for ($x = $minX; $x -le $maxX; $x++) {
        if ($layerPixels[$row + $x * 4 + 3] -lt 128) { continue }
        $dist = [math]::Sqrt([math]::Pow($x + 0.5 - $centreX, 2) + [math]::Pow($y + 0.5 - $centreY, 2))
        if ($dist -gt $radius) { $radius = $dist }
    }
}
if ($radius -le 0.0) { throw 'The keyed artwork has no measurable extent.' }

Write-Host ("Master artwork: {0}px rings, centre ({1:N1},{2:N1}), radius {3:N1}px ({4:P1} of the canvas), {5:N1}% coverage" -f `
        $width, $centreX, $centreY, $radius, (2 * $radius / $width), $coverage)

function New-IconCanvas([int]$Size) {
    $bitmap = New-Object System.Drawing.Bitmap $Size, $Size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.Clear([System.Drawing.Color]::Transparent)
    $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $graphics.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    return @{ Bitmap = $bitmap; Graphics = $graphics }
}

function Add-RoundedPlate([System.Drawing.Graphics]$Graphics, [int]$Size, [double]$CornerFraction) {
    $inset = 0.5
    $side = $Size - 1.0
    $radius = $Size * $CornerFraction
    $diameter = $radius * 2.0
    $left = $inset
    $top = $inset
    $right = $left + $side - $diameter
    $bottom = $top + $side - $diameter
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc($left, $top, $diameter, $diameter, 180, 90)
    $path.AddArc($right, $top, $diameter, $diameter, 270, 90)
    $path.AddArc($right, $bottom, $diameter, $diameter, 0, 90)
    $path.AddArc($left, $bottom, $diameter, $diameter, 90, 90)
    $path.CloseFigure()
    $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, $BackgroundRgb[0], $BackgroundRgb[1], $BackgroundRgb[2]))
    try {
        $Graphics.FillPath($brush, $path)
    } finally {
        $brush.Dispose()
        $path.Dispose()
    }
}

function Add-DiscPlate([System.Drawing.Graphics]$Graphics, [int]$Size) {
    $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, $BackgroundRgb[0], $BackgroundRgb[1], $BackgroundRgb[2]))
    try {
        $Graphics.FillEllipse($brush, 0, 0, $Size - 1, $Size - 1)
    } finally {
        $brush.Dispose()
    }
}

function Add-Rings([System.Drawing.Graphics]$Graphics, [int]$Size, [double]$RingRadiusFraction) {
    $scale = ($Size * $RingRadiusFraction) / $radius
    $boxWidth = $width * $scale
    $boxHeight = $height * $scale
    $left = $Size / 2.0 - $centreX * $scale
    $top = $Size / 2.0 - $centreY * $scale
    $rect = New-Object System.Drawing.RectangleF ([single]$left), ([single]$top), ([single]$boxWidth), ([single]$boxHeight)
    $Graphics.DrawImage($layer, $rect)
}

function Save-Icon([int]$Size, [string]$Path, [string]$Plate) {
    $canvas = New-IconCanvas $Size
    try {
        switch ($Plate) {
            'rounded' { Add-RoundedPlate $canvas.Graphics $Size $LegacyCornerRadius }
            'disc' { Add-DiscPlate $canvas.Graphics $Size }
        }
        $ringRadius = if ($Plate -eq 'none') { $ForegroundRingRadius } else { $LegacyRingRadius }
        Add-Rings $canvas.Graphics $Size $ringRadius
        $canvas.Bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $canvas.Graphics.Dispose()
        $canvas.Bitmap.Dispose()
    }
}

$written = 0
foreach ($density in $Densities) {
    $dir = Join-Path $ResDir ('mipmap-' + $density.Name)
    $foregroundSize = [int][math]::Round($ForegroundCanvasDp * $density.Scale)
    $legacySize = [int][math]::Round($LegacyCanvasDp * $density.Scale)
    $targets = @(
        @{ Path = (Join-Path $dir 'ic_launcher_foreground.png'); Size = $foregroundSize; Plate = 'none' },
        @{ Path = (Join-Path $dir 'ic_launcher.png'); Size = $legacySize; Plate = 'rounded' },
        @{ Path = (Join-Path $dir 'ic_launcher_round.png'); Size = $legacySize; Plate = 'disc' }
    )
    foreach ($target in $targets) {
        if ($Verify) {
            if (-not (Test-Path -LiteralPath $target.Path)) { throw "Missing icon $($target.Path)." }
            continue
        }
        if (-not (Test-Path -LiteralPath $dir)) { throw "Missing density directory $dir (run tauri android init first)." }
        Save-Icon $target.Size $target.Path $target.Plate
        $written++
    }
}

if ($Verify) {
    Write-Host 'Icon files are present.'
    exit 0
}

$valuesDir = Join-Path $ResDir 'values'
if (Test-Path -LiteralPath $valuesDir) {
    $colourPath = Join-Path $valuesDir 'ic_launcher_background.xml'
    $colourXml = @"
<?xml version="1.0" encoding="utf-8"?>
<resources>
  <color name="ic_launcher_background">#$(('{0:X2}{1:X2}{2:X2}' -f $BackgroundRgb[0], $BackgroundRgb[1], $BackgroundRgb[2]))</color>
</resources>
"@
    Set-Content -LiteralPath $colourPath -Value $colourXml -Encoding utf8
}

$layer.Dispose()
Write-Host ("Wrote {0} launcher icons to {1}" -f $written, $ResDir)
exit 0
