<#
  Baseline for benchmarks: the leanest pipeline plain FFmpeg can do.
  Desktop Duplication capture (stays on the GPU) -> NVENC -> file.
  No scenes, no preview, no overlays. Compare MilerCast and OBS against this.

  Usage:
    powershell -ExecutionPolicy Bypass -File .\tools\ffmpeg-baseline.ps1
    powershell -ExecutionPolicy Bypass -File .\tools\ffmpeg-baseline.ps1 -Seconds 300 -Mic "Microphone (3- Shure MV7)"
  Press q in the window to stop early.
#>
param(
    [int]$Seconds = 120,
    [int]$Fps = 60,                      # output frame rate (not the game's)
    [int]$Monitor = 0,
    [string]$Bitrate = "8M",
    [string]$Mic = "",                   # names: ffmpeg -list_devices true -f dshow -i dummy
    [string]$Priority = "BelowNormal",   # the game always gets the CPU first
    [string]$OutDir = "$PSScriptRoot\recordings"
)

New-Item -ItemType Directory -Force $OutDir | Out-Null
$out = Join-Path $OutDir ("baseline-{0:yyyyMMdd-HHmmss}.mkv" -f (Get-Date))

$ffArgs = @(
    '-hide_banner', '-loglevel', 'warning', '-stats',
    # 1) Capture: frames arrive as GPU textures, never copied to RAM
    '-f', 'lavfi', '-i', "ddagrab=output_idx=${Monitor}:framerate=${Fps}:draw_mouse=0"
)
if ($Mic) {
    $ffArgs += @('-f', 'dshow', '-audio_buffer_size', '50', '-i', "audio=$Mic")
}
$ffArgs += @(
    # 2) Encode: NVENC reads the GPU texture directly (zero-copy)
    '-c:v', 'h264_nvenc', '-preset', 'p4', '-tune', 'll',
    '-rc', 'cbr', '-b:v', $Bitrate, '-g', ($Fps * 2), '-bf', '0'
)
if ($Mic) { $ffArgs += @('-c:a', 'aac', '-b:a', '160k') }
# 3) Output: mkv survives being stopped half-way
$ffArgs += @('-t', $Seconds, '-y', $out)

$argLine = ($ffArgs | ForEach-Object { if ("$_" -match '\s') { '"' + $_ + '"' } else { "$_" } }) -join ' '

Write-Host "Recording $Seconds s from monitor $Monitor -> $out" -ForegroundColor Cyan
Write-Host "Play your game now. Watch 'drop=' below: it should stay 0." -ForegroundColor Cyan
$p = Start-Process ffmpeg -ArgumentList $argLine -NoNewWindow -PassThru
try { $p.PriorityClass = $Priority } catch { }
$p.WaitForExit()

if (Test-Path $out) { Write-Host "`nSaved: $out" -ForegroundColor Green }
