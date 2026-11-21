# Checks the add-in's ribbon tab: shows an Excel of its own for a quarter of a minute, finds the
# Epiphany tab, and presses Begin, Roll back, Begin, Commit and Refresh as a person would,
# watching the status bar and a cell for what each does. Run after
#   cargo build -p epiphany-server
#   dotnet build adapter/Epiphany.Excel -c Release
# The add-in must be looking for the server at its default address, and port 8080 must be
# free. Unlike the other checks this one puts a window on the screen; leave it alone while
# it runs.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$repo = Split-Path $PSScriptRoot
$port = 8080
$address = "http://127.0.0.1:$port"
$directory = Join-Path ([IO.Path]::GetTempPath()) "epiphany-excel-ribbon-$PID"
$xll = "$repo\adapter\Epiphany.Excel\bin\Release\net472\publish\Epiphany64-packed.xll"

if (Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue) { throw "port $port is in use; stop the server that has it first" }
New-Item -ItemType Directory $directory | Out-Null
Copy-Item -Recurse "$repo\crates\epiphany-core\tests\fixtures\model" "$directory\model"

$server = $null; $excel = $null; $excelPid = [uint32]0; $failures = 1
try {
    $server = Start-Process -PassThru -WindowStyle Hidden "$repo\target\debug\epiphany-server.exe" -ArgumentList 'serve', "`"$directory`"", "127.0.0.1:$port"
    Start-Sleep -Seconds 2
    if ($server.HasExited) { throw 'the server did not start' }

    $excel = New-Object -ComObject Excel.Application
    # Remember which process this instance is, so that only it is ever stopped.
    Add-Type -Namespace Native -Name Window -MemberDefinition '[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);'
    [void][Native.Window]::GetWindowThreadProcessId([IntPtr]$excel.Hwnd, [ref]$excelPid)
    $excel.DisplayAlerts = $false
    "registered: $($excel.RegisterXLL($xll))"
    $book = $excel.Workbooks.Add()
    $cell = $book.Worksheets.Item(1).Cells.Item(1, 1)
    $cell.Formula = '=DBRW("General Ledger","Rent","UK","Jan","Actual")'
    $excel.Visible = $true
    Start-Sleep -Seconds 4

    $automation = [Windows.Automation.AutomationElement]
    $window = $automation::FromHandle([IntPtr]$excel.Hwnd)
    function Find([Windows.Automation.ControlType]$type, [string]$name) {
        $typed = New-Object Windows.Automation.PropertyCondition $automation::ControlTypeProperty, $type
        $named = New-Object Windows.Automation.PropertyCondition $automation::NameProperty, $name
        $window.FindFirst([Windows.Automation.TreeScope]::Descendants, (New-Object Windows.Automation.AndCondition $typed, $named))
    }
    # Excel answers nothing while a command runs, so every look at it waits its turn.
    function Excel([scriptblock]$look) {
        for ($tries = 0; $tries -lt 40; $tries++) { try { return & $look } catch { Start-Sleep -Milliseconds 250 } }
        throw 'Excel stayed busy'
    }
    function Press([string]$button) {
        $found = Find ([Windows.Automation.ControlType]::Button) $button
        if (-not $found) { throw "the ribbon has no button '$button'" }
        $found.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
        Start-Sleep -Seconds 2
    }
    function Until([scriptblock]$look, [string]$expected) {
        for ($tries = 0; $tries -lt 15; $tries++) {
            $seen = [string](Excel $look)
            if ($seen -eq $expected) { return $seen }
            Start-Sleep -Seconds 1
        }
        return $seen
    }
    $bar = { if ($excel.StatusBar -is [string] -and $excel.StatusBar -like 'Epiphany: in a transaction*') { 'in a transaction' } else { 'none' } }
    $shown = { $cell.Text }
    $stored = {
        $ask = '{"cells":[{"cube":"General Ledger","at":["Rent","UK","Jan","Actual"]}]}'
        [double](Invoke-RestMethod -Method Post -Uri "$address/cells/read" -ContentType 'application/json' -Body $ask).cells[0].value
    }

    $tab = Find ([Windows.Automation.ControlType]::TabItem) 'Epiphany'
    if (-not $tab) { throw 'the ribbon has no Epiphany tab' }
    $tab.GetCurrentPattern([Windows.Automation.SelectionItemPattern]::Pattern).Select()
    Start-Sleep -Seconds 2

    # Which of the transaction's buttons can be pressed, as B(egin), C(ommit), R(oll back).
    $able = {
        $can = foreach ($button in 'Begin', 'Commit', 'Roll back') {
            if ((Find ([Windows.Automation.ControlType]::Button) $button).Current.IsEnabled) { $button.Substring(0, 1) }
        }
        -join $can
    }

    $steps = @()
    $steps += @(,@('the formula answers', (Until $shown '0'), '0'))
    $steps += @(,@('with no transaction only Begin can be pressed', (Until $able 'B'), 'B'))
    Press 'Begin'
    $steps += @(,@('Begin opens a transaction', (Until $bar 'in a transaction'), 'in a transaction'))
    $steps += @(,@('and then only Commit and Roll back can be', (Until $able 'CR'), 'CR'))
    Excel { $cell.Select() | Out-Null; $cell.Value2 = [double]12 }
    $steps += @(,@('a value typed in it shows', (Until $shown '12'), '12'))
    Press 'Roll back'
    $steps += @(,@('Roll back ends it', (Until $bar 'none'), 'none'))
    $steps += @(,@('and the value is gone', (Until $shown '0'), '0'))
    Press 'Begin'
    Excel { $cell.Select() | Out-Null; $cell.Value2 = [double]34 }
    $steps += @(,@('a value typed in another shows', (Until $shown '34'), '34'))
    Press 'Commit'
    $steps += @(,@('Commit ends it', (Until $bar 'none'), 'none'))
    $steps += @(,@('and Begin can be pressed again', (Until $able 'B'), 'B'))
    $steps += @(,@('and the server holds the value', [string](& $stored), '34'))
    # Changed behind Excel's back, the cell follows only when Refresh is pressed.
    $write = '{"cells":[{"cube":"General Ledger","at":["Rent","UK","Jan","Actual"],"value":56}]}'
    Invoke-WebRequest -UseBasicParsing -Method Post -Uri "$address/cells/write" -ContentType 'application/json' -Body $write | Out-Null
    Start-Sleep -Seconds 2
    $steps += @(,@('a change elsewhere is not shown yet', [string](Excel $shown), '34'))
    Press 'Refresh'
    $steps += @(,@('Refresh fetches it', (Until $shown '56'), '56'))

    $failures = 0
    foreach ($step in $steps) {
        $ok = $step[1] -eq $step[2]
        if (-not $ok) { $failures++ }
        "{0}  {1} -> <{2}>" -f $(if ($ok) { 'ok  ' } else { 'FAIL' }), $step[0], $step[1]
    }
    "$failures failed"
}
finally {
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
exit $failures
