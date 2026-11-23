# Shared by the checks that run the add-in inside Excel; each loads it with a dot. It gives
# them a server on a copy of the example model and an Excel of their own with the add-in
# loaded, and takes both away again. Nothing here touches the sample or any Excel but its own.
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot
# The add-in's default address, which is where it looks unless Connect has named another.
$port = 8080
$address = "http://127.0.0.1:$port"
$binary = "$repo\target\debug\epiphany-server.exe"
$xll = "$repo\adapter\Epiphany.Excel\bin\Release\net472\publish\Epiphany64-packed.xll"
$directory = Join-Path ([IO.Path]::GetTempPath()) "epiphany-excel-check-$PID"
$server = $null; $excel = $null; $excelPid = [uint32]0; $failures = 1

# A folder of its own with a copy of the example model, for the server to keep its data in.
function New-Home {
    if (Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue) { throw "port $port is in use; stop the server that has it first" }
    New-Item -ItemType Directory $directory | Out-Null
    Copy-Item -Recurse "$repo\crates\epiphany-core\tests\fixtures\model" "$directory\model"
}

function Start-Server {
    $script:server = Start-Process -PassThru -WindowStyle Hidden $binary -ArgumentList 'serve', "`"$directory`"", "127.0.0.1:$port"
    Start-Sleep -Seconds 2
    if ($server.HasExited) { throw 'the server did not start' }
}

# An Excel of the check's own, hidden, with the add-in loaded.
function Start-Excel {
    $script:excel = New-Object -ComObject Excel.Application
    # Remember which process this instance is, so that only it is ever stopped.
    Add-Type -Namespace Native -Name Window -MemberDefinition '[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);'
    $found = [uint32]0
    [void][Native.Window]::GetWindowThreadProcessId([IntPtr]$excel.Hwnd, [ref]$found)
    $script:excelPid = $found
    $excel.DisplayAlerts = $false
    "registered: $($excel.RegisterXLL($xll))"
}

# Ends the Excel and the server and removes the folder, whatever state the check left them in.
function Stop-All {
    if ($excel) {
        try { $excel.Visible = $false; foreach ($open in @($excel.Workbooks)) { $open.Close($false) }; $excel.Quit() } catch {}
        [void][Runtime.InteropServices.Marshal]::ReleaseComObject($excel)
    }
    Start-Sleep -Seconds 2
    if ($excelPid) { Get-Process -Id $excelPid -ErrorAction SilentlyContinue | Where-Object Name -eq 'EXCEL' | Stop-Process -Force -ErrorAction SilentlyContinue }
    if ($server -and -not $server.HasExited) { Stop-Process -Id $server.Id -Force }
    Start-Sleep -Seconds 1
    Remove-Item -Recurse -Force $directory -ErrorAction SilentlyContinue
}
