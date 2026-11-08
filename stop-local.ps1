$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
try {
    & wsl.exe -d Ubuntu -- bash scripts/stop-local.sh
    exit $LASTEXITCODE
} finally { Pop-Location }
