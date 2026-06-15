# Epiphany Excel add-in

A thin Excel client for an Epiphany server (ADR-0022). It is **not** a second
engine: it reads and writes cells over the REST API and lets the server do all
the calculation, security, and storage.

- Live cube values in cells with `=EPIPHANY.READ(...)`.
- A friendly Connect screen that reuses the server's own login (hosted in
  WebView2); the session token is stored encrypted to your Windows account
  (DPAPI), never in the workbook.
- One-click, transactional write-back of an edited table.
- A what-if sandbox box, mirroring the web client.

## Requirements

- Windows with Excel (64-bit recommended).
- The [WebView2 runtime](https://developer.microsoft.com/microsoft-edge/webview2/)
  (preinstalled on current Windows; the Connect screen needs it).
- A reachable Epiphany server.

## Build

From this directory:

```
dotnet build -c Release
```

The packed, single-file add-ins are written to
`bin/Release/net8.0-windows/publish/`:

- `Epiphany64-packed.xll` - for 64-bit Excel (the usual choice).
- `Epiphany-packed.xll` - for 32-bit Excel.

These bundle the managed code and the WebView2 loader, so the one `.xll` is all
you distribute.

## Install (per user)

1. Copy the matching `*-packed.xll` somewhere stable.
2. In Excel: **File > Options > Add-ins > Manage: Excel Add-ins > Go... >
   Browse...**, pick the `.xll`, and enable it. (Or just double-click the `.xll`
   and allow it for the session.)
3. An **Epiphany** ribbon tab appears.

## Use

1. **Connect** - click it, enter your server address (e.g.
   `https://localhost:8443`), and sign in on the page that loads. The token is
   saved encrypted for next time; **Sign out** clears it.
2. **Read** - in any cell:

   ```
   =EPIPHANY.READ("Sales", "Region=North", "Measure=Amount")
   ```

   The cube name first, then one `Dimension=Member` per dimension. The value
   updates asynchronously (the formula shows briefly as calculating).

3. **Write back** - lay out a small table and select it, then click **Commit
   selection**:

   | Region | Measure | Value |
   | ------ | ------- | ----- |
   | North  | Amount  | 100   |
   | South  | Amount  | 250   |

   The first row names the dimensions, with `Value` as the last column; each
   following row is one coordinate and its value. Commit sends the whole table
   as **one transaction** (all-or-nothing) to the cube you pick. Only leaf
   coordinates are writable; the server rejects a non-leaf write and nothing is
   applied.

4. **Sandbox** - type a what-if sandbox name in the ribbon box to read and write
   through it; clear it to work on base data.

## Verification status

This add-in is **built and statically verified** in the project's CI-style gate
(`dotnet build` is clean and the REST contract matches the server). Excel itself
is not present in that environment, so the in-Excel behavior (ribbon, UDF recalc,
WebView2 login, commit) is **load-tested by you in Excel** - see ADR-0022 for why
this boundary exists.

## Notes / follow-ups

- Code-signing the `.xll` for trusted distribution, an RTD push channel for live
  updates instead of manual recalc, and richer report-template range mapping are
  tracked as follow-ups in ADR-0022.
