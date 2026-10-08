[CmdletBinding()]
param(
    [switch] $SkipBuild,
    [switch] $SkipInstaller,
    [string] $AppVersion,
    [string] $InnoCompiler
)
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path "$PSScriptRoot/..").Path
. "$PSScriptRoot/windows-env.ps1"
Push-Location $repoRoot
try {
    if (-not $SkipBuild) {
        & cargo build --release --locked --target x86_64-pc-windows-msvc -p tidemark -p tidemarkd --bins
        if ($LASTEXITCODE -ne 0) { throw "Cargo build failed: $LASTEXITCODE" }
    }
    if (-not $AppVersion) {
        $manifest = Get-Content -LiteralPath "$repoRoot/Cargo.toml" -Raw
        $AppVersion = [regex]::Match($manifest, '(?ms)^\[workspace\.package\].*?^version = "([^"]+)"').Groups[1].Value
    }
    if ($AppVersion -notmatch '^\d+\.\d+\.\d+$') { throw "Invalid installer version: $AppVersion" }
    & "$repoRoot/data/packaging/windows/stage-runtime.ps1"
    if ($SkipInstaller) { return }
    if (-not $InnoCompiler) { $InnoCompiler = & "$repoRoot/data/packaging/windows/install-inno.ps1" }
    $outputDir = Join-Path $repoRoot 'build/windows/installer'
    New-Item -ItemType Directory -Path $outputDir -Force | Out-Null
    & $InnoCompiler "/DPayloadDir=$repoRoot/build/windows/payload" "/DAppVersion=$AppVersion" "/DOutputDir=$outputDir" "$repoRoot/data/packaging/windows/installer.iss"
    if ($LASTEXITCODE -ne 0) { throw "Inno compile failed: $LASTEXITCODE" }
    $setup = Join-Path $outputDir "Tidemark-v$AppVersion-setup.exe"
    if (-not (Test-Path -LiteralPath $setup)) { throw "Setup output missing: $setup" }
    Write-Host "Setup: $setup"
} finally { Pop-Location }
