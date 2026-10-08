[CmdletBinding()]
param([string] $ToolsDir = "$PSScriptRoot/../../../build/windows/tools")
$ErrorActionPreference = 'Stop'
# Official release asset digest: https://api.github.com/repos/jrsoftware/issrc/releases/tags/is-7_1_0
$version = '7.1.0'
$sha256 = '0362a383ed217d4c4239b5933866dd96d3eb2102737da92f80f6057a4b40df2f'
$ToolsDir = [IO.Path]::GetFullPath($ToolsDir)
New-Item -ItemType Directory -Path $ToolsDir -Force | Out-Null
$archive = Join-Path $ToolsDir "innosetup-$version-x64.exe"
if (-not (Test-Path -LiteralPath $archive)) {
    Invoke-WebRequest -Uri "https://github.com/jrsoftware/issrc/releases/download/is-7_1_0/innosetup-$version-x64.exe" -OutFile $archive
}
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $sha256) { throw 'Inno Setup installer SHA-256 mismatch.' }
$destination = Join-Path $ToolsDir "inno-$version"
$compiler = Join-Path $destination 'ISCC.exe'
if (-not (Test-Path -LiteralPath $compiler)) {
    $process = Start-Process -FilePath $archive -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/CURRENTUSER', "/DIR=`"$destination`"") -WindowStyle Hidden -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "Inno Setup install failed: $($process.ExitCode)" }
}
if (-not (Test-Path -LiteralPath $compiler)) { throw "Inno compiler missing: $compiler" }
$actualVersion = & $compiler --version
if ($LASTEXITCODE -ne 0 -or "$actualVersion".Trim() -ne $version) { throw "Expected Inno Setup $version; got $actualVersion" }
return $compiler
