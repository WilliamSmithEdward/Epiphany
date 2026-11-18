# Lab

A place to try the server by hand.

## Start

Double-click `start.cmd`. It builds the server, starts it on the model in `model/` and opens
the lab page at <http://127.0.0.1:8080/lab>. Close the window to stop the server.

From a terminal at the repository root, the same thing is:

```
cargo run --release -p epiphany-server -- lab lab
```

## What is here

| Path | Purpose |
| --- | --- |
| `model/` | The lab's model: `dimensions/*.json` and `cubes/*.json`. Edit freely, then restart. |
| `index.html` | The lab page. It is read on every request, so edits show on refresh. |
| `start.cmd` | The launcher. |
| `.env.example` | Settings you can override by copying it to `.env.local`. |

`.env.local` is ignored by git; put anything machine-specific or sensitive there.

## Using the page

- Pick a cube, then drag its dimensions between four zones:
  - **Columns** and **Rows** lay the dimension's elements across the top or down the side.
    Several dimensions in one zone are nested.
  - **Filters** narrows the dimension to the one element you choose.
  - **Unused** leaves the dimension at its top element, usually its grand total.
- White cells are leaves: type a value and press Enter or leave the cell to write it.
- Shaded cells are consolidations, calculated by the server on every refresh.
- **Fill visible leaves** writes random numbers to every numeric leaf on screen in one batch.
- **Begin** opens a transaction. Until you **Commit**, your writes are visible only on this
  page; open the lab in a second tab to see that others do not see them. **Roll back**
  discards them. A transaction left idle for five minutes is rolled back by the server.

Data lives in memory only for now, so it is gone when the server stops.
