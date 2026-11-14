# Builds a zip and an Inno Setup installer for x64 into dist\.
# Needs Inno Setup 6 (choco install innosetup).
$ErrorActionPreference = "Stop"
$root = (Resolve-Path "$PSScriptRoot\..\..").Path
Set-Location $root
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches[0].Groups[1].Value

cargo build --release --locked -p sql-honcho
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Remove-Item -Recurse -Force dist -ErrorAction SilentlyContinue
$name = "sql-honcho-$version-windows-x64"
New-Item -ItemType Directory -Path "dist\$name" | Out-Null
Copy-Item target\release\sql-honcho.exe, LICENSE, README.md "dist\$name"
Compress-Archive -Path "dist\$name\*" -DestinationPath "dist\$name.zip"
Remove-Item -Recurse "dist\$name"

$iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
& $iscc /Q "/DVersion=$version" "/DRoot=$root" "/DOutDir=$root\dist" packaging\windows\sql-honcho.iss
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
