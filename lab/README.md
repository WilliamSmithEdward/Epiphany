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
| `data/` | Created on first start: the stored cells. Not committed. |
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
- Dimensions on columns and rows are shown as trees that fold:
  - The arrow beside a consolidation folds or unfolds that one element.
  - The buttons on a dimension's chip fold that dimension: `++` expands all, `+` expands
    one level, `−` collapses one level, `−−` collapses all.
  - **Expand all** and **Collapse all** in the top bar do so for every dimension.
- Blue cells are leaves: type a value and press Enter or leave the cell to write it.
- Grey cells are consolidations, calculated by the server on every refresh.
- **Fill visible leaves** writes random numbers to every numeric leaf on screen in one batch.
- **Begin** opens a transaction. Until you **Commit**, your writes are visible only on this
  page; open the lab in a second tab to see that others do not see them. **Roll back**
  discards them. A transaction left idle for five minutes is rolled back by the server.

## Data

What you write is kept in `data/` (ignored by git) and is still there after a restart. Delete
the `data` folder to start again from an empty cube.

If you edit the model files by hand, cells that no longer fit the model are dropped at the
next start, with a warning in the server window. Updating the model through the API rewrites
the files in `model/` and keeps the previous version under `data/models/`.
