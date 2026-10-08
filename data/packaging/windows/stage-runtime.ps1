[CmdletBinding()]
param(
    [string] $ReleaseDir,
    [string] $PayloadDir,
    [switch] $FunctionsOnly
)
$ErrorActionPreference = 'Stop'

function Get-PeImportsFromText([string] $Text) {
    $names = @([regex]::Matches($Text, '(?im)^\s+([a-z0-9_+-][a-z0-9_.+-]*\.[a-z0-9_+-]+)\s*$') | ForEach-Object { $_.Groups[1].Value.ToLowerInvariant() } | Sort-Object -Unique)
    if ($names.Count -eq 0) { throw 'No PE imports found in dumpbin output.' }
    return $names
}

function Assert-SystemImports([string] $Binary, [string[]] $Imports) {
    # Explicit OS component allowlist, rather than accepting anything that happens
    # to have been installed into System32 by another application.
    $system = @('advapi32.dll', 'avrt.dll', 'bcrypt.dll', 'bcryptprimitives.dll', 'cfgmgr32.dll', 'comctl32.dll', 'comdlg32.dll', 'combase.dll', 'crypt32.dll', 'd2d1.dll', 'd3d11.dll', 'd3d12.dll', 'd3dcompiler_47.dll', 'dbghelp.dll', 'dcomp.dll', 'dnsapi.dll', 'dwmapi.dll', 'dwrite.dll', 'dxgi.dll', 'gdi32.dll', 'hid.dll', 'imm32.dll', 'iphlpapi.dll', 'kernel32.dll', 'ksuser.dll', 'msimg32.dll', 'msvcrt.dll', 'ncrypt.dll', 'netapi32.dll', 'ntdll.dll', 'ole32.dll', 'oleaut32.dll', 'opengl32.dll', 'powrprof.dll', 'propsys.dll', 'psapi.dll', 'rpcrt4.dll', 'secur32.dll', 'setupapi.dll', 'shell32.dll', 'shlwapi.dll', 'user32.dll', 'uiautomationcore.dll', 'userenv.dll', 'usp10.dll', 'uxtheme.dll', 'version.dll', 'winhttp.dll', 'wininet.dll', 'winmm.dll', 'winnsi.dll', 'winspool.drv', 'wlanapi.dll', 'ws2_32.dll', 'wtsapi32.dll')
    foreach ($name in $Imports) {
        $lower = $name.ToLowerInvariant()
        if ($lower -match '^(vcruntime|msvcp|ucrtbase|api-ms-win-crt)') { throw "Dynamic CRT import $name required by $Binary" }
        if ($lower -notin $system -and $lower -notmatch '^(api-ms-win-|ext-ms-win-)') { throw "Unexpected non-system import $name required by $Binary" }
    }
}

function Assert-SafePayloadPath([string] $RepoRoot, [string] $Candidate) {
    $path = [IO.Path]::GetFullPath($Candidate)
    $allowed = [IO.Path]::GetFullPath((Join-Path $RepoRoot 'build/windows'))
    if (-not $path.StartsWith($allowed + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Payload directory must be inside $allowed"
    }
    # Check every existing ancestor before a recursive removal. A lexically safe
    # path through a junction can otherwise refer to files outside the workspace.
    $ancestor = $path
    while ($ancestor) {
        if (Test-Path -LiteralPath $ancestor) {
            $item = Get-Item -LiteralPath $ancestor -Force
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Payload path contains a reparse point: $ancestor" }
        }
        $parent = [IO.Directory]::GetParent($ancestor)
        $ancestor = if ($parent) { $parent.FullName } else { $null }
    }
    return $path
}

if ($FunctionsOnly) { return }
$repoRoot = (Resolve-Path "$PSScriptRoot/../../..").Path
if (-not $ReleaseDir) { $ReleaseDir = Join-Path $repoRoot 'target/x86_64-pc-windows-msvc/release' }
if (-not $PayloadDir) { $PayloadDir = Join-Path $repoRoot 'build/windows/payload' }
$ReleaseDir = (Resolve-Path -LiteralPath $ReleaseDir).Path
$PayloadDir = Assert-SafePayloadPath $repoRoot $PayloadDir
$buildRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'build/windows'))
$importsLog = @()
foreach ($name in @('tidemark.exe', 'tidemarkd.exe', 'tidemark-maintenance.exe')) {
    $binary = Join-Path $ReleaseDir $name
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "Missing release binary: $binary" }
    $output = & dumpbin.exe /NOLOGO /DEPENDENTS $binary 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { throw "dumpbin failed for $binary`: $output" }
    $imports = @(Get-PeImportsFromText $output)
    Assert-SystemImports -Binary $binary -Imports $imports
    $importsLog += "$name`: $($imports -join ' ')"
    $version = [Diagnostics.FileVersionInfo]::GetVersionInfo($binary)
    if (-not $version.ProductVersion -or $version.ProductName -ne 'Tidemark') { throw "Missing Tidemark version resource: $binary" }
}
if (Test-Path -LiteralPath $PayloadDir) { Remove-Item -LiteralPath $PayloadDir -Recurse -Force }
New-Item -ItemType Directory -Path "$PayloadDir/share/icons" -Force | Out-Null
foreach ($name in @('tidemark.exe', 'tidemarkd.exe', 'tidemark-maintenance.exe')) {
    Copy-Item -LiteralPath (Join-Path $ReleaseDir $name) -Destination $PayloadDir
}
Copy-Item -LiteralPath "$repoRoot/data/icons/hicolor" -Destination "$PayloadDir/share/icons" -Recurse
Copy-Item -LiteralPath "$repoRoot/data/icons/tidemark.ico" -Destination "$PayloadDir/share/tidemark.ico"
Copy-Item -LiteralPath "$repoRoot/LICENSE" -Destination $PayloadDir
Copy-Item -LiteralPath "$repoRoot/docs/TRADEMARKS.md" -Destination $PayloadDir
$files = @(Get-ChildItem -LiteralPath $PayloadDir -File -Recurse | ForEach-Object { [IO.Path]::GetRelativePath($PayloadDir, $_.FullName).Replace('\', '/') } | Sort-Object)
ConvertTo-Json -InputObject $files | Set-Content -LiteralPath "$PayloadDir/install-manifest.json" -Encoding utf8NoBOM
$importsLog | Set-Content -LiteralPath "$buildRoot/pe-imports.txt" -Encoding utf8NoBOM
Write-Host "Staged $($files.Count) owned files; all three binaries import Windows components only."
