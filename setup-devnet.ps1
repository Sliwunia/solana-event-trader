[CmdletBinding()]
param([switch]$Trigger)
$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
try {
    $setupArguments = @('-d', 'Ubuntu', '--', 'bash', 'scripts/setup-devnet.sh')
    if ($Trigger) { $setupArguments += '--trigger' }
    & wsl.exe @setupArguments
    exit $LASTEXITCODE
} finally { Pop-Location }
