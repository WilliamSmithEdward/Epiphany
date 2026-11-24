# Checks the add-in's own windows inside Excel: starts a server with one made-up user on a copy
# of the example model, loads the add-in into a hidden Excel of its own, and lets a second
# process type into the windows as a person would. Into the Connect window goes first a wrong
# password, then the right one; in the window that chooses a SUBNM's element the list is
# narrowed by typing and the element left is chosen. Run after
#   cargo build -p epiphany-server
#   dotnet build adapter/Epiphany.Excel -c Release
# The add-in must be looking for the server at its default address, port 8080 must be free,
# and there must be no settings file yet: the check removes the one that signing in saves.
param([string]$Drive, [string]$Step)

$ErrorActionPreference = 'Stop'
$window = 'Connect to Epiphany'

if ($Drive) {
    # The second process: finds the add-in's windows on the desktop and fills them in.
    Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
    $automation = [Windows.Automation.AutomationElement]
    function Find([string]$name) {
        $named = New-Object Windows.Automation.PropertyCondition $automation::NameProperty, $name
        for ($tries = 0; $tries -lt 120; $tries++) {
            $found = $automation::RootElement.FindFirst([Windows.Automation.TreeScope]::Children, $named)
            if ($found) { return $found }
            Start-Sleep -Milliseconds 250
        }
        throw "no window named '$name' appeared"
    }
    # A window of a hidden Excel offers no automation patterns, so the controls are sent the
    # plain messages a keyboard and mouse would cause: set the text, click.
    Add-Type -Namespace Native -Name Control -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessage(IntPtr window, uint message, IntPtr first, string text);
[DllImport("user32.dll")] public static extern bool PostMessage(IntPtr window, uint message, IntPtr first, IntPtr second);
'@
    function Press($within, [string]$name) {
        $named = New-Object Windows.Automation.PropertyCondition $automation::NameProperty, $name
        $button = $within.FindFirst([Windows.Automation.TreeScope]::Descendants, $named)
        # Posted, not sent: the click opens another window, and must not be waited for.
        [void][Native.Control]::PostMessage([IntPtr]$button.Current.NativeWindowHandle, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)
    }
    function Fill([string]$password) {
        $form = Find $window
        $all = $form.FindAll([Windows.Automation.TreeScope]::Descendants, [Windows.Automation.Condition]::TrueCondition)
        # Environment, server, name, password, from the top down.
        $boxes = @($all | Where-Object { $_.Current.ClassName -like '*.EDIT.*' } | Sort-Object { $_.Current.BoundingRectangle.Top })
        $values = @($null, $null, $env:EPIPHANY_CHECK_NAME, $password)
        for ($box = 2; $box -lt 4; $box++) {
            [void][Native.Control]::SendMessage([IntPtr]$boxes[$box].Current.NativeWindowHandle, 0x000C, [IntPtr]::Zero, $values[$box])
        }
        $shown = $boxes[1].Current.Name
        Press $form 'Connect'
        return $shown
    }
    try {
        if ($Step -eq 'choose') {
            $form = Find 'Choose an element'
            $all = $form.FindAll([Windows.Automation.TreeScope]::Descendants, [Windows.Automation.Condition]::TrueCondition)
            $box = @($all | Where-Object { $_.Current.ClassName -like '*.EDIT.*' })[0]
            $list = @($all | Where-Object { $_.Current.ClassName -like '*.SysListView32.*' })[0]
            # LVM_GETITEMCOUNT: how many elements the list holds, before and after narrowing it.
            $before = [Native.Control]::SendMessage([IntPtr]$list.Current.NativeWindowHandle, 0x1004, [IntPtr]::Zero, $null)
            [void][Native.Control]::SendMessage([IntPtr]$box.Current.NativeWindowHandle, 0x000C, [IntPtr]::Zero, 'ma')
            Start-Sleep -Milliseconds 300
            $after = [Native.Control]::SendMessage([IntPtr]$list.Current.NativeWindowHandle, 0x1004, [IntPtr]::Zero, $null)
            Add-Content $Drive "list narrowed from $before to $after"
            Press $form 'OK'
            return
        }
        $server = Fill 'not-the-password'
        Add-Content $Drive "window opened, server shown as $server"
        Start-Sleep -Milliseconds 500
        Press (Find 'Epiphany') 'OK'
        Add-Content $Drive 'wrong password refused'
        Start-Sleep -Milliseconds 500
        [void](Fill $env:EPIPHANY_CHECK_PASSWORD)
        Add-Content $Drive 'right password entered'
    } catch { Add-Content $Drive "driver failed: $_" }
    return
}

. "$PSScriptRoot\check-common.ps1"
$log = "$directory\driver.log"
# Made up for this run; the user exists only in the scratch directory.
$env:EPIPHANY_CHECK_NAME = 'ada'
$env:EPIPHANY_CHECK_PASSWORD = 'test-only-' + [Guid]::NewGuid().ToString('N')

New-Home
Set-Content -Encoding ascii "$directory\password.txt" $env:EPIPHANY_CHECK_PASSWORD
Start-Process -Wait -WindowStyle Hidden $binary -ArgumentList 'user', "`"$directory`"", $env:EPIPHANY_CHECK_NAME -RedirectStandardInput "$directory\password.txt"
Remove-Item "$directory\password.txt"
Set-Content -Encoding ascii $log 'started'

$driver = $null
# Excel answers nothing while one of the add-in's windows is open, so every call waits its turn.
function Excel([scriptblock]$call) {
    for ($tries = 0; $tries -lt 60; $tries++) {
        try { return & $call } catch { Start-Sleep -Milliseconds 500 }
    }
    throw 'Excel stayed busy'
}
# What a cell shows, once it shows what is expected or time has run out.
function Shown([int]$row, [string]$expected) {
    for ($tries = 0; $tries -lt 20; $tries++) {
        Start-Sleep -Seconds 1
        $text = Excel { [string]$sheet.Cells.Item($row, 1).Text }
        if ($text -eq $expected) { return $text }
    }
    return $text
}
try {
    Start-Server
    Start-Excel
    $settings = Join-Path ([string]$excel.ExecuteExcel4Macro('GET.WORKSPACE(23)') -replace '\\Microsoft\\Excel\\XLSTART$', '') 'Epiphany\adapter.json'
    $book = $excel.Workbooks.Add()
    $sheet = $book.Worksheets.Item(1)

    $driver = Start-Process -PassThru -WindowStyle Hidden powershell -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Drive', "`"$log`""
    $read = '=DBRW("General Ledger","Rent","UK","{0}","Actual")'
    # The server refuses this for want of a session, which opens the Connect window.
    $sheet.Cells.Item(1, 1).Formula = $read -f 'Jan'
    $driver.WaitForExit(90000) | Out-Null
    Get-Content $log | Select-Object -Skip 1

    $checks = [ordered]@{}
    $checks['the window opened, a wrong password was refused, the right one accepted'] = @(Get-Content $log).Count -eq 4
    $checks['a formula answers once signed in'] = (Shown 1 '0') -eq '0'
    Excel { $sheet.Cells.Item(4, 1).Formula = '=TM1USER("")' }
    $checks['TM1USER gives the name signed in'] = (Shown 4 $env:EPIPHANY_CHECK_NAME) -eq $env:EPIPHANY_CHECK_NAME
    $checks['the address and the name were saved, not the password'] = [bool](Excel { $excel.ExecuteExcel4Macro("FILES(`"$settings`")") -is [string] })

    # Choosing an element: the list of the whole dimension, narrowed to the months with 'ma'.
    Excel { $sheet.Cells.Item(3, 1).Formula = '=SUBNM("Period","",1)' }
    $checks['a SUBNM shows the element at its index'] = (Shown 3 'H1') -eq 'H1'
    Excel { $sheet.Cells.Item(3, 1).Select() } | Out-Null
    Set-Content -Encoding ascii $log 'started'
    $driver = Start-Process -PassThru -WindowStyle Hidden powershell -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Drive', "`"$log`"", '-Step', 'choose'
    try { $excel.Run('Choose') | Out-Null } catch {}
    $driver.WaitForExit(60000) | Out-Null
    Get-Content $log | Select-Object -Skip 1
    $checks['the list held every element and typing narrowed it'] = (Get-Content $log)[-1] -eq 'list narrowed from 9 to 2'
    $checks['the element chosen is in the formula'] = (Excel { [string]$sheet.Cells.Item(3, 1).Formula }) -eq '=SUBNM("Period","","Mar")'
    $checks['and the cell shows it'] = (Shown 3 'Mar') -eq 'Mar'

    Excel { $excel.Run('Disconnect') } | Out-Null
    # Not asked for again unasked: after Disconnect a refused formula just shows #N/A.
    Excel { $sheet.Cells.Item(2, 1).Formula = $read -f 'Feb' }
    $refused = Shown 2 '#N/A'
    if ($refused -ne '#N/A') { "after Disconnect the formula shows <$refused>" }
    $checks['after Disconnect a formula is refused'] = $refused -eq '#N/A'
    $checks['and TM1USER gives no name'] = (Shown 4 '') -eq ''

    $failures = 0
    foreach ($check in $checks.GetEnumerator()) {
        if (-not $check.Value) { $failures++ }
        "{0}  {1}" -f $(if ($check.Value) { 'ok  ' } else { 'FAIL' }), $check.Key
    }
    "$failures failed"
    Excel { $book.Close($false) }
}
finally {
    if ($driver -and -not $driver.HasExited) { Stop-Process -Id $driver.Id -Force }
    # Signing in saved a settings file; Excel removes it, having written it.
    try { if ($excel -and $settings) { [void]$excel.ExecuteExcel4Macro("FILE.DELETE(`"$settings`")") } } catch {}
    Stop-All
}
exit $failures
