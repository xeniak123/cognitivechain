<#
.SYNOPSIS
    Pobiera koparkę CognitiveChain, weryfikuje sumę kontrolną i zaczyna kopać.

.EXAMPLE
    .\mine.ps1 -Wallet cog523fe4ffffb34e4dd244b2e2cc5a543e812ac802 -Pool 203.0.113.10

.NOTES
    Skrypt nigdy nie prosi o klucz prywatny. Do kopania wystarczy adres publiczny.
    Nie masz portfela? Skrypt pobiera też cog-node.exe:
        .\cog-node.exe keygen --out wallet.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Wallet,
    [Parameter(Mandatory = $true)][string]$Pool,
    [string]$Home_ = "$env:LOCALAPPDATA\CognitiveChain",
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Extra
)

$ErrorActionPreference = 'Stop'
$repo  = 'xeniak123/cognitivechain'
$label = 'windows-x64'
$asset = 'cog-miner.exe'
$node  = 'cog-node.exe'

function Say  ($m) { Write-Host "==> $m" -ForegroundColor White }
function Fail ($m) { Write-Host "!! $m" -ForegroundColor Red; exit 1 }

if ($Wallet -notmatch '^cog[0-9a-fA-F]{40}$') {
    Fail "adres portfela musi mieć postać cog + 40 znaków hex, dostałem: $Wallet"
}

New-Item -ItemType Directory -Force -Path $Home_ | Out-Null
Set-Location $Home_

if (-not (Test-Path (Join-Path $Home_ $asset))) {
    Say 'Pobieram koparkę z najnowszego wydania'
    $base = "https://github.com/$repo/releases/latest/download"
    try {
        Invoke-WebRequest -Uri "$base/$asset" -OutFile $asset -UseBasicParsing
    } catch {
        Fail "nie udało się pobrać $asset - sprawdź, czy wydanie jest już opublikowane"
    }
    try { Invoke-WebRequest -Uri "$base/$node" -OutFile $node -UseBasicParsing } catch { }
    $sumsFile = "SHA256SUMS-$label.txt"
    try {
        Invoke-WebRequest -Uri "$base/$sumsFile" -OutFile $sumsFile -UseBasicParsing
    } catch {
        Fail 'brak pliku z sumami kontrolnymi - przerywam, nie uruchomię niesprawdzonej binarki'
    }

    Say 'Sprawdzam sumę kontrolną'
    # Format `sha256sum`: <64 znaki hex><spacje>[*]<nazwa pliku>, jedna linia na plik.
    # Celowo nie akceptujemy niczego innego: nieznany format to odmowa
    # uruchomienia, a nie zgadywanie, która suma należy do której binarki.
    $expected = $null
    foreach ($line in (Get-Content $sumsFile)) {
        if ($line -match ('^([0-9a-fA-F]{64})\s+\*?' + [regex]::Escape($asset) + '\s*$')) {
            $expected = $Matches[1].ToLower()
            break
        }
    }
    if (-not $expected) {
        Fail "nie znalazlem sumy dla $asset w $sumsFile - przerywam zamiast uruchamiac niesprawdzony plik"
    }

    $actual = (Get-FileHash -Path $asset -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) {
        Remove-Item $asset -Force
        Fail "SUMA KONTROLNA SIĘ NIE ZGADZA. Plik skasowany. Oczekiwano $expected, jest $actual"
    }
    Write-Host "OK: $actual" -ForegroundColor Green
}

Say 'Startuję koparkę'
Write-Host "portfel: $Wallet"
Write-Host "węzeł:   $Pool"
Write-Host ''

$argv = @('--wallet', $Wallet, '--pool', $Pool)
if ($Extra) { $argv += $Extra }
& (Join-Path $Home_ $asset) @argv
exit $LASTEXITCODE
