[CmdletBinding()]
param([switch]$Background)
$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
try {
    if ($Background) {
        & wsl.exe -d Ubuntu -- bash scripts/start-local.sh --status
        if ($LASTEXITCODE -eq 0) {
            Write-Host 'Lokalny test juz dziala. Log: .tools/local-demo.log. Zatrzymanie: .\stop-local.ps1'
            exit 0
        }
        $localArguments = @('-d', 'Ubuntu', '--cd', ('"' + $PSScriptRoot + '"'), '--', 'bash', 'scripts/start-local.sh')
        New-Item -ItemType Directory -Force -Path '.tools' | Out-Null
        $localProcess = Start-Process -FilePath 'wsl.exe' -ArgumentList $localArguments -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput (Join-Path $PSScriptRoot '.tools/local-demo.log') `
            -RedirectStandardError (Join-Path $PSScriptRoot '.tools/local-demo-errors.log')
        Write-Host ('Uruchomiono lokalny test w tle. PID Windows: ' + $localProcess.Id)
        Write-Host 'Postep: Get-Content .tools/local-demo.log -Wait'
        Write-Host 'Zatrzymanie: .\stop-local.ps1'
    } else {
        & wsl.exe -d Ubuntu -- bash scripts/start-local.sh
        exit $LASTEXITCODE
    }
} finally { Pop-Location }
