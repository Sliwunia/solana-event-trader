[CmdletBinding()]
param(
    [string]$ConfigPath = 'config.toml',
    [switch]$Build,
    [switch]$BuildOnly,
    [switch]$Local,
    [switch]$Live
)

$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
try {
    if ($Local) {
        if ($Live -or $BuildOnly -or $Build) { throw 'Tryb -Local uruchamiaj osobno, bez -Live i opcji budowy.' }
        & wsl.exe -d Ubuntu -- bash scripts/start-local.sh
        exit $LASTEXITCODE
    }
    if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
        throw 'Brak WSL. Ten launcher wymaga dystrybucji Ubuntu w WSL.'
    }
    if ($Build -or $BuildOnly -or -not (Test-Path -LiteralPath 'dist/linux-x86_64/solana-event-trader')) {
        & wsl.exe -d Ubuntu -- bash scripts/build-wsl.sh
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    if ($BuildOnly) { exit 0 }

    if (-not (Test-Path -LiteralPath $ConfigPath)) {
        if ($ConfigPath -ne 'config.toml') { throw 'Nie znaleziono wskazanego pliku konfiguracji.' }
        Copy-Item -LiteralPath 'config.example.toml' -Destination 'config.toml'
        Write-Host 'Utworzono config.toml. Wpisz rzeczywiste adresy i sciezke portfela.'
    }
    $configFullPath = (Resolve-Path -LiteralPath $ConfigPath).Path
    $missingFields = @(
        foreach ($line in Get-Content -LiteralPath $configFullPath) {
            if ($line -match '^\s*([a-z_]+)\s*=\s*"REPLACE_') { $Matches[1] }
        }
    )
    if ($missingFields.Count -gt 0) {
        Write-Host 'Konfiguracja nie jest jeszcze gotowa. Brakuje:'
        $missingFields | ForEach-Object { Write-Host (' - ' + $_) }
        Write-Host ''
        Write-Host 'Nie zgaduj tych adresow. Dla darmowego testu uruchom:'
        Write-Host '  .\setup-devnet.ps1'
        Write-Host 'Konfigurator przygotuje osobny portfel, tokeny i pule na devnet.'
        Write-Host 'Jesli faucet odmowi testowego SOL, konfigurator poda jeden adres do zasilenia.'
        Write-Host ''
        Write-Host 'Test na tym komputerze BEZ FAUCETU I GITHUBA:'
        Write-Host '  .\start-local.ps1'
        exit 2
    }
    $wslConfigPath = & wsl.exe -d Ubuntu -- wslpath -u $configFullPath
    if ($LASTEXITCODE -ne 0) { throw 'Nie mozna przeliczyc sciezki konfiguracji dla WSL.' }
    $runArguments = @('-d', 'Ubuntu', '--', 'bash', 'scripts/run-wsl.sh', ([string]$wslConfigPath).Trim())
    if ($Live) {
        $runArguments += '--live'
        Write-Host 'Tryb LIVE: program moze wysylac rzeczywiste transakcje.'
    } else {
        Write-Host 'Tryb symulacji. Wysylanie transakcji jest wylaczone.'
    }
    & wsl.exe @runArguments
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
