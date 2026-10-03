# build.ps1 — FullSizePreview_H Rust プラグイン ビルド & デプロイ
#
# 使い方: .\build.ps1
#         .\build.ps1 -NoDeploy

param(
    [switch]$NoDeploy
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$SrcDir      = $PSScriptRoot
$AviUtl2Root = "C:\ProgramData\aviutl2"
$PluginDir   = "$AviUtl2Root\Plugin\FullSizePreview_H"

Write-Host "`n=== FullSizePreview_H ビルド (Rust) ===" -ForegroundColor Yellow

Push-Location $SrcDir
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed (exit $LASTEXITCODE)"
    }
} finally {
    Pop-Location
}

$DllPath = Join-Path $SrcDir "target\release\full_size_preview_h.dll"
if (-not (Test-Path $DllPath)) {
    Write-Error "ビルド成果物が見つかりません: $DllPath"
}

$Aux2Out = Join-Path $SrcDir "FullSizePreview_H.aux2"
Copy-Item -Path $DllPath -Destination $Aux2Out -Force
Write-Host "  完了: $Aux2Out" -ForegroundColor Green

if (-not $NoDeploy) {
    Write-Host "`n=== デプロイ ===" -ForegroundColor Yellow
    if (Get-Process aviutl2 -ErrorAction SilentlyContinue) {
        throw "AviUtl2 が起動中です。終了してから再実行してください（.aux2 を掴んでいて置き換えられません）"
    }
    if (-not (Test-Path $PluginDir)) {
        New-Item -ItemType Directory -Path $PluginDir -Force | Out-Null
    }
    Copy-Item -Path $Aux2Out -Destination "$PluginDir\FullSizePreview_H.aux2" -Force
    Write-Host "  配置: $PluginDir\FullSizePreview_H.aux2" -ForegroundColor DarkGreen
}

Write-Host "`n全ビルド完了`n" -ForegroundColor Yellow
