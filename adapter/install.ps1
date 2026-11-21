# Has Excel load the add-in every time it starts, for this user only: no administrator rights.
#
#   powershell -ExecutionPolicy Bypass -File adapter\install.ps1            install or update
#   powershell -ExecutionPolicy Bypass -File adapter\install.ps1 -Remove    take it out again
#
# It copies the add-in to %APPDATA%\Epiphany and names it in Excel's list of add-ins to open
# (the OPEN values under HKCU\Software\Microsoft\Office\16.0\Excel\Options). Close Excel first.
# -Xll names the add-in file, if it is not the one this repository builds.
param(
    [switch]$Remove,
    [string]$Xll,
    # Where Excel keeps its list; only the installer's own test gives another.
    [string]$Options = 'HKCU:\Software\Microsoft\Office\16.0\Excel\Options',
    [string]$Folder = (Join-Path $env:APPDATA 'Epiphany')
)
$ErrorActionPreference = 'Stop'
$installed = Join-Path $Folder 'Epiphany.xll'

# The add-ins Excel opens are the values OPEN, OPEN1, OPEN2 and so on, with no gaps.
function Listed {
    $names = @()
    if (Test-Path $Options) {
        $item = Get-Item $Options
        $names = @($item.Property | Where-Object { $_ -match '^OPEN\d*$' } |
            Sort-Object { [int]($_ -replace '^OPEN', '0') })
    }
    foreach ($name in $names) { [pscustomobject]@{ Name = $name; Value = [string](Get-ItemPropertyValue $Options $name) } }
}

# Writes the list back without gaps, so that Excel reads all of it.
function Relist($values) {
    foreach ($entry in Listed) { Remove-ItemProperty $Options $entry.Name }
    for ($index = 0; $index -lt $values.Count; $index++) {
        $name = if ($index -eq 0) { 'OPEN' } else { "OPEN$index" }
        New-ItemProperty $Options -Name $name -Value $values[$index] -PropertyType String | Out-Null
    }
}

$ours = '/R "' + $installed + '"'
$others = @(Listed | Where-Object { $_.Value -ne $ours } | ForEach-Object Value)

if ($Remove) {
    if (Test-Path $Options) { Relist $others }
    if (Test-Path $installed) { Remove-Item $installed }
    'Removed. Excel will no longer load the add-in.'
    return
}

if (-not $Xll) {
    # The add-in must match Excel: 64-bit Excel takes the file with 64 in its name.
    $bitness = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Office\ClickToRun\Configuration' -ErrorAction SilentlyContinue).Platform
    $name = if ($bitness -eq 'x86') { 'Epiphany-packed.xll' } else { 'Epiphany64-packed.xll' }
    $Xll = Join-Path $PSScriptRoot "Epiphany.Excel\bin\Release\net472\publish\$name"
}
if (-not (Test-Path $Xll)) {
    throw "$Xll is missing. Build it with 'dotnet build adapter/Epiphany.Excel -c Release', or name the file with -Xll."
}
New-Item -ItemType Directory -Force $Folder | Out-Null
Copy-Item $Xll $installed -Force
if (-not (Test-Path $Options)) { New-Item $Options -Force | Out-Null }
Relist ($others + $ours)
"Installed $installed. Start Excel; the Epiphany menu is on the Add-ins tab."
