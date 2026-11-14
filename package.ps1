# Builds the release and writes the deliverable dist\kibotos-v<version>.exe,
# named after the workspace version ("0.1.0" -> kibotos-v0.1.exe).
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

cargo build --release -p kibotos-app
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$line = Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
$version = $line.Matches[0].Groups[1].Value -replace '\.0$', ''
New-Item -ItemType Directory -Force dist | Out-Null
$out = "dist\kibotos-v$version.exe"
Copy-Item target\release\kibotos.exe $out -Force
$size = (Get-Item $out).Length
"{0}  ({1:N0} bytes)" -f $out, $size
