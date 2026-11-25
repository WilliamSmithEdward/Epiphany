# Excel adapter

An Excel add-in that gives worksheets the PAfE functions (`DBRW`, `DBS`, `SUBNM` and the
rest) against an Epiphany server. It holds no OLAP logic: every function is a question to
the server's API. What each function takes and returns is in
[docs/PAFE-COMPATIBILITY.md](../docs/PAFE-COMPATIBILITY.md).

## Get it

Every run of the repository's CI on GitHub keeps an `epiphany-windows` artifact with the
add-in (`Epiphany64-packed.xll` for 64-bit Excel) and `epiphany-server.exe`, so neither has to
be built to be tried. They are not signed.

To have Excel load it at every start, for your user only and without administrator rights,
close Excel and run `powershell -ExecutionPolicy Bypass -File adapter\install.ps1`; the same
with `-Remove` takes it out again.

## Build

Needs the .NET SDK. From the repository root:

```
dotnet build adapter/Epiphany.Excel -c Release
```

The add-in is then `adapter/Epiphany.Excel/bin/Release/net472/publish/Epiphany64-packed.xll`
for 64-bit Excel (`Epiphany-packed.xll` for 32-bit). It is one file and needs nothing
installed beside Excel, and no administrator rights.

## Use

1. Start a server, for example the sample with `start.cmd`, which starts the GUI beside it.
2. In Excel, open the `.xll` file (File, Open), and allow the add-in for the session.
3. Enter a formula:

   ```
   =DBRW("General Ledger", "Revenue", "Total Entity", "Q1", "Actual")
   ```

A cube or dimension can name its environment, as PAfE workbooks do: `"production:Sales"` is
the cube Sales on the server called production, and a plain `"Sales"` is looked for in the
default environment. To begin with there is one environment, `local`, at
`http://127.0.0.1:8080`. The **Epiphany** tab of the ribbon has a button for each of these;
they are also in an Epiphany menu on the Add-ins tab:

- **Connect...** to add an environment or change one (its name, its server, who signs in),
  and to sign in. The environment connected becomes the default.
- **Disconnect** to sign out of every environment.
- **Refresh** (Ctrl+Shift+R) to fetch every formula's value again.
- **Begin transaction**, **Commit** and **Roll back**. In a transaction, what you enter shows
  in your Excel and nowhere else until you commit; rolling back discards it. The server
  rolls back a transaction left unused for five minutes.

Double-clicking a `SUBNM` cell, or **Choose element...** in the menu, lists the elements of
its subset; type to narrow the list, and the one you choose is written into the formula.

Typing a value over a `DBRW` or `DBR` cell writes it to the cube and keeps the formula. If
the server refuses the value, because the cell is a consolidation, say, the status bar says so.
Once a value is written, every formula asks the server again, so totals over it follow.
Over a consolidation, type a code before the number to spread it over the leaves beneath, as
in the GUI's grid: `P1200` in proportion to what they hold, `S1200` (or `LS1200`) in equal
shares, `R100` (or `LR100`) to give each 100. `P+100` or `S~100` adds 100 to what the cell
holds, or takes it away, and spreads that; `P%10` makes it a tenth more, in proportion, and
`P%-10` a tenth less; `C` alone clears the leaves.

While a value is on its way a cell shows the value it had last, or `#GETTING_DATA` the first
time. It shows `#N/A` when the server cannot be reached or wants a sign-in, and `*KEY_ERR`
when the environment, the cube or an element it names does not exist.

## Test

```
cargo build -p epiphany-server
dotnet test adapter/Epiphany.Excel.Tests
```

The tests start a real server on a copy of the example model and ask it through the same
code the worksheet functions use. They do not need Excel.

To check the functions inside Excel itself:

```
dotnet build adapter/Epiphany.Excel -c Release
powershell -ExecutionPolicy Bypass -File adapter\check-excel.ps1
```

It loads the add-in into a hidden Excel of its own, enters a formula for each row of the
compatibility table against a server on a copy of the example model, and compares what the
cells show. Port 8080 must be free.

`adapter\check-excel-windows.ps1` does the same for the add-in's own windows, on a server with
one made-up user: a second process types a wrong password and then the right one into the
Connect window, narrows the list that chooses a `SUBNM`'s element and chooses one, and the
check then disconnects. It removes the settings file that signing in saves, so run it only
before you have connected to anything yourself.

`adapter\check-excel-ribbon.ps1` presses the buttons of the Epiphany tab: Begin, Roll back,
Commit and Refresh, watching the status bar and a cell for what each does. Unlike the other
two it shows its Excel for a quarter of a minute; leave the window alone while it runs.

The three share `adapter\check-common.ps1`, which starts the server and the Excel each of
them uses and takes both away again.

## Layout

| File | Purpose |
| --- | --- |
| `Epiphany.Excel/Functions.cs` | The worksheet functions. |
| `Epiphany.Excel/Connection.cs` | The server: batched cell reads and writes, dimensions, sign-in. |
| `Epiphany.Excel/Dimension.cs` | A copy of a dimension and the answers read from it. |
| `Epiphany.Excel/AddIn.cs` | The environments, the commands and the Connect window. |
| `Epiphany.Excel/Ribbon.cs` | The Epiphany tab of the ribbon. |
| `Epiphany.Excel/Writeback.cs` | Writing a value typed over a read formula. |
| `Epiphany.Excel/Picker.cs` | Choosing the element of a `SUBNM` formula. |
| `Epiphany.Excel/Formula.cs` | Reading a formula as a function call and its arguments. |
| `Epiphany.Excel/Json.cs` | JSON reading and writing. |
| `Epiphany.Excel/Numbers.cs` | Numbers read from text and written as text exactly, which the .NET Framework's own parsing and `"R"` format are not. |
| `Epiphany.Excel.Tests/` | The tests. |
