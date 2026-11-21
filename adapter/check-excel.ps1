# Checks the add-in inside Excel: starts a server on a copy of the example model, loads the
# add-in into a hidden Excel of its own, enters one formula per row of the compatibility table
# (docs/PAFE-COMPATIBILITY.md) and compares what the cells show. Run after
#   cargo build -p epiphany-server
#   dotnet build adapter/Epiphany.Excel -c Release
# The add-in must be looking for the server at its default address (it is, unless Connect has
# been used to name another), and port 8080 must be free.
. "$PSScriptRoot\check-common.ps1"
New-Home
Set-Content -Encoding ascii "$directory\model\dimensions\line.json" '{"name":"Line","type":"standard","elements":[{"name":"Comment","type":"string"},{"name":"Amount","type":"numeric","attributes":{"Short":"Amt"}}],"attributes":{"Short":"alias"},"subsets":{"Texts":{"elements":["Comment"]},"Backwards":{"elements":["Amount","Comment"]}}}'
Set-Content -Encoding ascii "$directory\model\cubes\notes.json" '{"name":"Notes","dimensions":["Entity","Line"]}'

try {
    Start-Server
    Start-Excel
    # The ribbon tab arrives as a COM add-in that Excel-DNA registers under the add-in's own
    # name; the tab is offered if that one is connected.
    $ribbon = @($excel.COMAddIns | Where-Object { $_.Description -eq 'Epiphany' -and $_.ProgId -like 'Dna_*' -and $_.Connect })
    if ($ribbon.Count -ne 1) { throw 'the ribbon did not load' }
    'ribbon: True'
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
        @('=SUBNM("Line","Nope",1)', '*KEY_ERR'),
        # Nobody is signed in to this server, so the name is empty.
        @('=TM1USER("local")', ''),
        @('=TM1USER("nowhere")', '*KEY_ERR'),
        @('=DNLEV("Period")', '3'),
        @('=DNEXT("Period","H1")', 'Q1'),
        @('=DNEXT("Period","Jun")', ''),
        @('=DTYPE("Period","Q1")', 'C'),
        @('=DTYPE("Line","Comment")', 'S'),
        @('=ELISCOMP("Period","Jan","Q1")', 'TRUE'),
        @('=ELISCOMP("Period","Jan","H1")', 'FALSE'),
        @('=ELISPAR("Period","Q1","Jan")', 'TRUE'),
        @('=ELISANC("Period","H1","Jan")', 'TRUE'),
        @('=ELWEIGHT("Account","Net Income","Operating Expenses")', '-1'),
        @('=SUBSIZ("Line","Backwards")', '2'),
        @('=SUBSIZ("Line","Nope")', '*KEY_ERR'),
        # A total over the cell that is typed over further down.
        @("=DBRW($gl,`"Operating Expenses`",`"Total Entity`",`"H1`",`"Budget`")", '0')
    )
    for ($row = 0; $row -lt $cases.Count; $row++) { $sheet.Cells.Item($row + 1, 1).Formula = $cases[$row][0] }

    function Shown { for ($row = 0; $row -lt $cases.Count; $row++) { [string]$sheet.Cells.Item($row + 1, 1).Text } }
    function Settle {
        # Waits until no cell is still fetching its value.
        for ($tries = 0; $tries -lt 30; $tries++) {
            Start-Sleep -Seconds 1
            $now = @(Shown)
            $waiting = 0
            for ($row = 0; $row -lt $cases.Count; $row++) { if ($now[$row] -eq '#GETTING_DATA') { $waiting++ } }
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
    # While the new value is fetched the cell goes on showing the old one.
    for ($tries = 0; $tries -lt 15 -and [string]$sheet.Cells.Item(5, 1).Text -ne '42'; $tries++) { Start-Sleep -Seconds 1 }
    $fresh = [string]$sheet.Cells.Item(5, 1).Text
    $ok = $stale -eq '0' -and $fresh -eq '42'
    if (-not $ok) { $failed++ }
    "{0}  Refresh: before <{1}> after <{2}>" -f $(if ($ok) { 'ok  ' } else { 'FAIL' }), $stale, $fresh

    # Typing over a read formula writes the value to the cube and keeps the formula.
    $typed = @(
        @(5, 77, '77', 'a number over a leaf'),
        @(3, 5, '150.5', 'a number over a consolidation is refused'),
        # The last formula is the total over the leaf that now holds 77.
        @($cases.Count, 'P154', '154', 'a letter and a number over a consolidation spread the number'),
        @(5, 'p77', '77', 'in either case, and over a leaf too'),
        @(8, 12, '12', 'a number over a string cell is written as text'),
        @(8, 'S100', 'S100', 'what looks like a spread is text in a string cell'),
        @(8, 'hello', 'hello', 'text over a string cell')
    )
    foreach ($case in $typed) {
        $cell = $sheet.Cells.Item($case[0], 1)
        $cell.Select() | Out-Null
        if ($case[1] -is [string]) { $cell.Value2 = [string]$case[1] } else { $cell.Value2 = [double]$case[1] }
        for ($tries = 0; $tries -lt 10; $tries++) { Start-Sleep -Seconds 1; if ([string]$cell.Text -eq $case[2]) { break } }
        $ok = [string]$cell.Text -eq $case[2] -and ([string]$cell.Formula).StartsWith('=DBRW(')
        if (-not $ok) { $failed++ }
        "{0}  Typed: {1} -> <{2}> <{3}>" -f $(if ($ok) { 'ok  ' } else { 'FAIL' }), $case[3], $cell.Text, $cell.Formula
        if ($case[0] -eq 5) {
            # The total over that leaf follows without being asked to.
            $total = $sheet.Cells.Item($cases.Count, 1)
            for ($tries = 0; $tries -lt 10 -and [string]$total.Text -ne '77'; $tries++) { Start-Sleep -Seconds 1 }
            $ok = [string]$total.Text -eq '77'
            if (-not $ok) { $failed++ }
            "{0}  Typed: a total over that leaf follows -> <{1}>" -f $(if ($ok) { 'ok  ' } else { 'FAIL' }), $total.Text
        }
    }

    # A transaction: what is typed in it shows in Excel and nowhere else, until it is
    # committed; rolled back, the sheet goes back to what is stored.
    $leaf = $sheet.Cells.Item(5, 1)
    $stored = {
        $ask = '{"cells":[{"cube":"General Ledger","at":["Rent","UK","Jun","Budget"]}]}'
        [string][double](Invoke-RestMethod -Method Post -Uri "$address/cells/read" -ContentType 'application/json' -Body $ask).cells[0].value
    }
    $shows = { param($expected) for ($tries = 0; $tries -lt 15 -and [string]$leaf.Text -ne $expected; $tries++) { Start-Sleep -Seconds 1 }; [string]$leaf.Text }
    $steps = @()
    $excel.Run('BeginTransaction') | Out-Null
    $leaf.Select() | Out-Null; $leaf.Value2 = [double]55
    $steps += @(,@('typed in a transaction, Excel shows it', (& $shows '55'), '55'))
    $steps += @(,@('and the server still holds the old value', (& $stored), '77'))
    $excel.Run('RollbackTransaction') | Out-Null
    $steps += @(,@('rolled back, Excel shows the old value', (& $shows '77'), '77'))
    $excel.Run('BeginTransaction') | Out-Null
    $leaf.Select() | Out-Null; $leaf.Value2 = [double]66
    $steps += @(,@('typed in another, Excel shows it', (& $shows '66'), '66'))
    $excel.Run('CommitTransaction') | Out-Null
    $steps += @(,@('committed, the server holds it', (& $stored), '66'))
    $steps += @(,@('and Excel still shows it', (& $shows '66'), '66'))
    foreach ($step in $steps) {
        $ok = $step[1] -eq $step[2]
        if (-not $ok) { $failed++ }
        "{0}  Transaction: {1} -> <{2}>" -f $(if ($ok) { 'ok  ' } else { 'FAIL' }), $step[0], $step[1]
    }

    # A change of the model is noticed at the add-in's next request, without Refresh: a month
    # is added behind Excel's back, and one new read of a cell makes the count of months
    # follow. A dimension function alone would not do: it is answered from the copy held.
    # First let the refresh that followed the commit finish, or its answers would be the ones
    # to bring the news.
    Start-Sleep -Seconds 6
    $model = Invoke-RestMethod "$address/model"
    $period = $model.model.dimensions | Where-Object name -eq 'Period'
    $period.elements += [pscustomobject]@{ name = 'Jul'; type = 'numeric' }
    $put = @{ based_on = $model.version; dimension = $period } | ConvertTo-Json -Depth 20
    Invoke-RestMethod -Method Put -Uri "$address/dimensions/Period" -ContentType 'application/json' -Body $put | Out-Null
    $row = 0
    for ($index = 0; $index -lt $cases.Count; $index++) { if ($cases[$index][0] -eq '=DIMSIZ("Period")') { $row = $index + 1 } }
    $size = $sheet.Cells.Item($row, 1)
    $before = [string]$size.Text
    $added = $sheet.Cells.Item($cases.Count + 1, 1)
    $added.Formula = '=DBRW("General Ledger","Rent","US","Jul","Actual")'
    for ($tries = 0; $tries -lt 15 -and ([string]$size.Text -ne '10' -or [string]$added.Text -ne '0'); $tries++) { Start-Sleep -Seconds 1 }
    $ok = $before -eq '9' -and [string]$size.Text -eq '10' -and [string]$added.Text -eq '0'
    if (-not $ok) { $failed++ }
    "{0}  Model change: months counted <{1}> then <{2}>, the new month reads <{3}>" -f $(if ($ok) { 'ok  ' } else { 'FAIL' }), $before, $size.Text, $added.Text

    # A value typed over a read goes to the cell that formula reads, wherever the formula
    # stands by then. Two reads on a sheet of their own, one with its month in another cell;
    # a row is inserted above them, which moves them without their being calculated again;
    # then the first is typed over.
    $moved = $book.Worksheets.Add()
    $moved.Cells.Item(1, 1).Value2 = 'Feb'
    $moved.Cells.Item(2, 1).Formula = "=DBRW($gl,`"Rent`",`"US`",A1,`"Budget`")"
    $moved.Cells.Item(3, 1).Formula = "=DBRW($gl,`"Salaries`",`"US`",`"Feb`",`"Budget`")"
    for ($tries = 0; $tries -lt 15 -and ([string]$moved.Cells.Item(2, 1).Text -ne '0' -or [string]$moved.Cells.Item(3, 1).Text -ne '0'); $tries++) { Start-Sleep -Seconds 1 }
    $moved.Rows.Item(1).Insert() | Out-Null
    $rent = $moved.Cells.Item(3, 1)
    $rent.Select() | Out-Null
    $rent.Value2 = [double]11
    for ($tries = 0; $tries -lt 15 -and [string]$rent.Text -ne '11'; $tries++) { Start-Sleep -Seconds 1 }
    $held = {
        param($account)
        $ask = @{ cells = @(@{ cube = 'General Ledger'; at = @($account, 'US', 'Feb', 'Budget') }) } | ConvertTo-Json -Depth 4
        [string][double](Invoke-RestMethod -Method Post -Uri "$address/cells/read" -ContentType 'application/json' -Body $ask).cells[0].value
    }
    $got = "{0} / rent {1} / salaries {2} / {3}" -f $rent.Text, (& $held 'Rent'), (& $held 'Salaries'), $rent.Formula
    $ok = $got -eq "11 / rent 11 / salaries 0 / =DBRW($gl,`"Rent`",`"US`",A2,`"Budget`")"
    if (-not $ok) { $failed++ }
    "{0}  Typed: over a formula that a new row had moved -> <{1}>" -f $(if ($ok) { 'ok  ' } else { 'FAIL' }), $got

    # The month may stand on another sheet, whose name Excel then writes in single quotes
    # where it has a space or a sign in it. A comma and a bracket in that name are the
    # name's and not the formula's.
    $named = $book.Worksheets.Add()
    $named.Name = 'Q1, plan (old'
    $named.Cells.Item(1, 1).Value2 = 'Mar'
    $far = $moved.Cells.Item(6, 1)
    $far.Formula = "=DBRW($gl,`"Salaries`",`"US`",'Q1, plan (old'!A1,`"Budget`")"
    for ($tries = 0; $tries -lt 15 -and [string]$far.Text -ne '0'; $tries++) { Start-Sleep -Seconds 1 }
    $moved.Activate() | Out-Null
    $far.Select() | Out-Null
    $far.Value2 = [double]13
    for ($tries = 0; $tries -lt 15 -and [string]$far.Text -ne '13'; $tries++) { Start-Sleep -Seconds 1 }
    $ask = @{ cells = @(@{ cube = 'General Ledger'; at = @('Salaries', 'US', 'Mar', 'Budget') }) } | ConvertTo-Json -Depth 4
    $stored = [string][double](Invoke-RestMethod -Method Post -Uri "$address/cells/read" -ContentType 'application/json' -Body $ask).cells[0].value
    $got = "{0} / stored {1} / {2}" -f $far.Text, $stored, $far.Formula
    $ok = $got -eq "13 / stored 13 / =DBRW($gl,`"Salaries`",`"US`",'Q1, plan (old'!A1,`"Budget`")"
    if (-not $ok) { $failed++ }
    "{0}  Typed: over a formula that reads from a sheet with a comma in its name -> <{1}>" -f $(if ($ok) { 'ok  ' } else { 'FAIL' }), $got
    "$failed failed"
    $script:failures = $failed
    $book.Close($false)
}
finally {
    Stop-All
}

exit $failures
