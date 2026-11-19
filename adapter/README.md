# Excel adapter

An Excel add-in that gives worksheets the PAfE functions (`DBRW`, `DBS`, `SUBNM` and the
rest) against an Epiphany server. It holds no OLAP logic: every function is a question to
the server's API. What each function takes and returns is in
[docs/PAFE-COMPATIBILITY.md](../docs/PAFE-COMPATIBILITY.md).

## Build

Needs the .NET SDK. From the repository root:

```
dotnet build adapter/Epiphany.Excel -c Release
```

The add-in is then `adapter/Epiphany.Excel/bin/Release/net472/publish/Epiphany64-packed.xll`
for 64-bit Excel (`Epiphany-packed.xll` for 32-bit). It is one file and needs nothing
installed beside Excel, and no administrator rights.

## Use

1. Start a server, for example the lab: `lab\start.cmd`.
2. In Excel, open the `.xll` file (File, Open), and allow the add-in for the session.
3. Enter a formula:

   ```
   =DBRW("General Ledger", "Revenue", "Total Entity", "Q1", "Actual")
   ```

A cube or dimension can name its environment, as PAfE workbooks do: `"production:Sales"` is
the cube Sales on the server called production, and a plain `"Sales"` is looked for in the
default environment. To begin with there is one environment, `local`, at
`http://127.0.0.1:8080`. The **Epiphany** menu, on Excel's Add-ins tab, has:

- **Connect...** to add an environment or change one (its name, its server, who signs in),
  and to sign in. The environment connected becomes the default.
- **Disconnect** to sign out of every environment.
- **Refresh** (Ctrl+Shift+R) to fetch every formula's value again.

Double-clicking a `SUBNM` cell, or **Choose element...** in the menu, lists the elements of
its subset; type to narrow the list, and the one you choose is written into the formula.

Typing a value over a `DBRW` or `DBR` cell writes it to the cube and keeps the formula. If
the server refuses the value, because the cell is a consolidation, say, the status bar says so.

A cell shows `#N/A` while its value is on its way and when the server cannot be reached, and
`*KEY_ERR` when the cube or an element it names does not exist.

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

## Layout

| File | Purpose |
| --- | --- |
| `Epiphany.Excel/Functions.cs` | The worksheet functions. |
| `Epiphany.Excel/Connection.cs` | The server: batched cell reads and writes, dimensions, sign-in. |
| `Epiphany.Excel/Dimension.cs` | A copy of a dimension and the answers read from it. |
| `Epiphany.Excel/AddIn.cs` | The environments, the menu commands and the Connect window. |
| `Epiphany.Excel/Writeback.cs` | Writing a value typed over a read formula. |
| `Epiphany.Excel/Picker.cs` | Choosing the element of a `SUBNM` formula. |
| `Epiphany.Excel/Formula.cs` | Reading a formula as a function call and its arguments. |
| `Epiphany.Excel/Json.cs` | JSON reading and writing. |
| `Epiphany.Excel.Tests/` | The tests. |
