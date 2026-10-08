$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/stage-runtime.ps1" -FunctionsOnly

function Assert-Rejected([scriptblock] $Action, [string] $Expected) {
    try { & $Action } catch {
        if ($_.Exception.Message -notlike "*$Expected*") { throw }
        return
    }
    throw "Expected rejection: $Expected"
}

$imports = @(Get-PeImportsFromText @'
    Section contains the following imports:
        KERNEL32.dll
        api-ms-win-core-synch-l1-2-0.dll
        USER32.dll
        winspool.drv
    Summary
'@)
if ($imports.Count -ne 4) { throw 'dumpbin import parsing lost module names' }
if (@(Get-PeImportsFromText "    custom.plugin").Count -ne 1) { throw 'Non-DLL dependency extension was skipped' }
Assert-SystemImports -Binary 'fixture.exe' -Imports $imports
foreach ($dll in @('libsqlite3-0.dll', 'libgcc_s_seh-1.dll', 'libstdc++-6.dll', 'msys-2.0.dll', 'VCRUNTIME140.dll', 'MSVCP140.dll', 'ucrtbase.dll', 'unexpected.dll', 'custom.plugin')) {
    Assert-Rejected { Assert-SystemImports -Binary 'fixture.exe' -Imports @($dll) } $dll
}
Assert-Rejected { Get-PeImportsFromText 'unexpected dumpbin output' } 'No PE imports'
$repoRoot = (Resolve-Path "$PSScriptRoot/../../..").Path
Assert-Rejected { Assert-SafePayloadPath $repoRoot (Join-Path $repoRoot 'data') } 'must be inside'
$testPath = Join-Path $repoRoot "build/windows/stage-path-test-$([Guid]::NewGuid().ToString('N'))"
$outside = Join-Path ([IO.Path]::GetTempPath()) "Tidemark.StageOutside.$([Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $testPath, $outside -Force | Out-Null
Set-Content -LiteralPath "$outside/sentinel.txt" -Value 'preserve'
try {
    $junction = Join-Path $testPath 'junction'
    New-Item -ItemType Junction -Path $junction -Target $outside | Out-Null
    Assert-Rejected { Assert-SafePayloadPath $repoRoot "$junction/payload" } 'reparse'
    if (-not (Test-Path -LiteralPath "$outside/sentinel.txt")) { throw 'Path check modified external target' }
} finally {
    if (Test-Path -LiteralPath "$testPath/junction") { Remove-Item -LiteralPath "$testPath/junction" -Force }
    # Both cleanup roots were explicitly created above and checked as absolute paths.
    if (-not ([IO.Path]::GetFullPath($testPath)).StartsWith((Join-Path $repoRoot 'build/windows') + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Invalid test cleanup root' }
    Remove-Item -LiteralPath $testPath -Recurse -Force
    if ([IO.Directory]::GetParent($outside).FullName -ne [IO.Path]::GetTempPath().TrimEnd('\')) { throw 'Invalid temporary cleanup root' }
    Remove-Item -LiteralPath $outside -Recurse -Force
}
Write-Host 'Native staging import checks passed.'
