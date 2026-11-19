# Checks the add-in inside Excel: starts a server on a copy of the example model, loads the
# add-in into a hidden Excel of its own, enters one formula per row of the compatibility table
# (docs/PAFE-COMPATIBILITY.md) and compares what the cells show. Run after
#   cargo build -p epiphany-server
#   dotnet build adapter/Epiphany.Excel -c Release
# The add-in must be looking for the server at its default address (it is, unless Connect has
# been used to name another), and port 8080 must be free.
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot
$port = 8080
$address = "http://127.0.0.1:$port"
$directory = Join-Path ([IO.Path]::GetTempPath()) "epiphany-excel-check-$PID"
$xll = "$repo\adapter\Epiphany.Excel\bin\Release\net472\publish\Epiphany64-packed.xll"

if (Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue) { throw "port $port is in use; stop the server that has it first" }
New-Item -ItemType Directory $directory | Out-Null
Copy-Item -Recurse "$repo\crates\epiphany-core\tests\fixtures\model" "$directory\model"
Set-Content -Encoding ascii "$directory\model\dimensions\line.json" '{"name":"Line","type":"standard","elements":[{"name":"Comment","type":"string"},{"name":"Amount","type":"numeric","attributes":{"Short":"Amt"}}],"attributes":{"Short":"alias"},"subsets":{"Texts":{"elements":["Comment"]},"Backwards":{"elements":["Amount","Comment"]}}}'
Set-Content -Encoding ascii "$directory\model\cubes\notes.json" '{"name":"Notes","dimensions":["Entity","Line"]}'

$server = $null; $excel = $null; $excelPid = [uint32]0
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
    $sheet = $book.Worksheets.Item(1)
    $sheet.Columns.Item(1).ColumnWidth = 40

    $gl = '"General Ledger"'
    $cases = @(
        @("=DBS(100,$gl,`"Product Revenue`",`"US`",`"Jan`",`"Actual`")", '100'),
        @("=DBSW(50.5,`"local:General Ledger`",`"servicerevenue`",`"uk`",`"M02`",`"ACTUAL`")", '50.5'),
        @("=DBRW($gl,`"Revenue`",`"Total Entity`",`"Q1`",`"Actual`")", '150.5'),
        @("=DBR($gl,`"Product Revenue`",`"US`",`"Jan`",`"Actual`")", '100'),
        @("=DBRW($gl,`"Rent`",`"UK`",`"Jun`",`"Budget`")", '0'),
        @('=DBSS("late","Notes","US","Comment")', 'late'),
        @('=DBRW("Notes","US","Comment")', 'late'),
        @('=DBRW("Notes","UK","Comment")', ''),
        @("=DBRW($gl,`"Rent`",`"UK`",`"Smarch`",`"Actual`")", '*KEY_ERR'),
        @('=DBRW("No Such Cube","a")', '*KEY_ERR'),
        @("=DBRW($gl,`"Rent`",`"UK`")", '*KEY_ERR'),
        @("=DBS(1,$gl,`"Revenue`",`"US`",`"Mar`",`"Actual`")", '*KEY_ERR'),
        @('=DBS(3,"Notes","UK","Comment")', '*KEY_ERR'),
        @("=DBS(`"x`",$gl,`"Rent`",`"UK`",`"Jan`",`"Actual`")", '#VALUE!'),
        @("=DBRW($gl,`"Rent`",`"UK`",NA(),`"Actual`")", '#N/A'),
        @("=DBRW($gl,`"Rent`",`"UK`",DIMNM(`"Period`",4),`"Actual`")", '0'),
        @('=DIMNM("Period",4)', 'Jan'),
        @('=DIMNM("Period",4,"Code")', 'M01'),
        @('=DIMNM("Period",99)', ''),
        @('=DIMIX("Period","M01")', '4'),
        @('=DIMIX("Period","Smarch")', '0'),
        @('=DIMSIZ("Period")', '9'),
        @('=DIMSIZ("LOCAL:Period")', '9'),
        @('=DIMSIZ("nowhere:Period")', '*KEY_ERR'),
        @('=DBRW("nowhere:General Ledger","Rent","UK","Jan","Actual")', '*KEY_ERR'),
        @('=DIMSIZ("Planet")', '*KEY_ERR'),
        @('=ELLEV("Period","H1")', '2'),
        @('=ELLEV("Period","Smarch")', '*KEY_ERR'),
        @('=ELPAR("Period","Feb",1)', 'Q1'),
        @('=ELPAR("Period","H1",1)', ''),
        @('=ELPARN("Period","Feb")', '1'),
        @('=ELCOMP("Period","Q2",2)', 'May'),
        @('=ELCOMPN("Period","Q2")', '3'),
        @('=DBRA("Period","Feb","Days")', '28'),
        @('=DBRA("Period","Feb","Code")', 'M02'),
        @('=DBRA("Period","Q1","Code")', 'Q1'),
        @('=ATTRS("Period","Feb","Days")', '28'),
        @('=ATTRN("Period","Feb","Days")', '28'),
        @('=ATTRN("Period","Feb","Code")', '*KEY_ERR'),
        @('=SUBNM("Line","Backwards",1)', 'Amount'),
        @('=SUBNM("Line","Backwards",1,"Short")', 'Amt'),
        @('=SUBNM("Line","Texts","comment")', 'Comment'),
        @('=SUBNM("Line","Texts","Amount")', ''),
        @('=SUBNM("Line","",1)', 'Comment'),
        @('=SUBNM("Line","Nope",1)', '*KEY_ERR')
    )
    for ($row = 0; $row -lt $cases.Count; $row++) { $sheet.Cells.Item($row + 1, 1).Formula = $cases[$row][0] }

    function Shown { for ($row = 0; $row -lt $cases.Count; $row++) { [string]$sheet.Cells.Item($row + 1, 1).Text } }
    function Settle {
        # Waits until no cell but the one that is meant to shows #N/A.
        for ($tries = 0; $tries -lt 30; $tries++) {
            Start-Sleep -Seconds 1
            $now = @(Shown)
            $waiting = 0
            for ($row = 0; $row -lt $cases.Count; $row++) { if ($now[$row] -eq '#N/A' -and $cases[$row][1] -ne '#N/A') { $waiting++ } }
            if ($waiting -eq 0) { return $now }
        }
        return $now
    }

    $now = Settle
    $failed = 0
    for ($row = 0; $row -lt $cases.Count; $row++) {
        $ok = $now[$row] -eq $cases[$row][1]
        if (-not $ok) { $failed++ }
        "{0}  {1}  -> <{2}>{3}" -f $(if ($ok) { 'ok  ' } else { 'FAIL' }), $cases[$row][0], $now[$row], $(if ($ok) { '' } else { "  expected <$($cases[$row][1])>" })
    }

    # Refresh: change a cell behind Excel's back; the formula shows it only after Refresh.
    $body = '{"cells":[{"cube":"General Ledger","at":["Rent","UK","Jun","Budget"],"value":42}]}'
    Invoke-WebRequest -UseBasicParsing -Method Post -Uri "$address/cells/write" -ContentType 'application/json' -Body $body | Out-Null
    Start-Sleep -Seconds 1
    $stale = [string]$sheet.Cells.Item(5, 1).Text
    $excel.Run('Refresh') | Out-Null
    $now = Settle
    $fresh = $now[4]
    $ok = $stale -eq '0' -and $fresh -eq '42'
    if (-not $ok) { $failed++ }
    "{0}  Refresh: before <{1}> after <{2}>" -f $(if ($ok) { 'ok  ' } else { 'FAIL' }), $stale, $fresh
    "$failed failed"
    $script:failures = $failed
    $book.Close($false)
}
finally {
    if ($excel) { try { $excel.Quit() } catch {} ; [void][Runtime.InteropServices.Marshal]::ReleaseComObject($excel) }
    Start-Sleep -Seconds 2
    if ($excelPid) { Get-Process -Id $excelPid -ErrorAction SilentlyContinue | Where-Object Name -eq 'EXCEL' | Stop-Process -Force -ErrorAction SilentlyContinue }
    if ($server -and -not $server.HasExited) { Stop-Process -Id $server.Id -Force }

    Start-Sleep -Seconds 1
    Remove-Item -Recurse -Force $directory -ErrorAction SilentlyContinue
}

exit $failures
