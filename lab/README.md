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
  - Right-click a dimension's chip in Columns or Rows for a menu that folds that dimension:
    expand all, expand one level, collapse one level, collapse all.
  - **Expand all** and **Collapse all** in the top bar do so for every dimension.
- Blue cells are leaves and work like spreadsheet cells:
  - Click selects a cell; the arrow keys move the selection, passing over consolidations.
  - Typing replaces the cell's value. F2 or a double click edits the value it has.
  - Enter writes the change and keeps the cell selected. Leaving the cell also writes it.
    Escape abandons the change. Delete clears the selected cell.
- Yellow cells have a change that is not final yet: either typed but not entered, or
  written inside a transaction that has not been committed.
- Grey cells are consolidations, calculated by the server on every refresh.
- **Fill visible leaves** writes random numbers to every numeric leaf on screen in one batch.
- **Begin** opens a transaction. Until you **Commit**, your writes are visible only on this
  page; open the lab in a second tab to see that others do not see them. **Roll back**
  discards them. A transaction left idle for five minutes is rolled back by the server.

## MDX

Below the grid is a box for MDX queries. **Run query** sends it to the server and shows the
answer as a table; hover a heading for the member's unique name, type and level. **Write the
view above as MDX** fills the box with a query for exactly what the grid shows, which is a
good starting point to edit. Inside a transaction, queries see its pending writes.

## Data

What you write is kept in `data/` (ignored by git) and is still there after a restart. Delete
the `data` folder to start again from an empty cube.

If you edit the model files by hand, cells that no longer fit the model are dropped at the
next start, with a warning in the server window. Updating the model through the API rewrites
the files in `model/` and keeps the previous version under `data/models/`.
