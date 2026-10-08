# Dot-source this file before native Cargo, dumpbin, or resource compilation.
[CmdletBinding()]
param([string] $VisualStudioPath)
$ErrorActionPreference = 'Stop'

if (-not $VisualStudioPath) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
    if (Test-Path -LiteralPath $vswhere) {
        $VisualStudioPath = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    }
}
if (-not $VisualStudioPath) { throw 'Visual Studio with the x64 C++ build tools is required.' }
$devShell = Join-Path $VisualStudioPath 'Common7/Tools/Launch-VsDevShell.ps1'
if (-not (Test-Path -LiteralPath $devShell)) { throw "Visual Studio developer shell missing: $devShell" }
& $devShell -Arch amd64 -HostArch amd64 -SkipAutomaticLocation | Out-Null
$env:RUSTUP_TOOLCHAIN = 'stable-x86_64-pc-windows-msvc'
# MSYS2 paths can otherwise select GNU link.exe, cmake, or pkg-config.
$env:PATH = (($env:PATH -split ';' | Where-Object { $_ -notmatch '[\\/]msys64[\\/]' }) -join ';')
foreach ($name in @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'PKG_CONFIG_PATH', 'PKG_CONFIG_LIBDIR')) {
    # .NET keeps empty environment variables on Windows; Cargo interprets an
    # empty RUSTFLAGS as an override of target-specific flags. Delete them.
    Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
}
foreach ($tool in @('cl.exe', 'link.exe', 'rc.exe', 'dumpbin.exe', 'cargo.exe')) {
    $resolved = Get-Command $tool -ErrorAction Stop
    Write-Host "$tool`: $($resolved.Source)"
}
