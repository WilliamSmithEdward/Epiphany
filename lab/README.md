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
- Grey cells are consolidations, added up by the server on every refresh. Typing a value
  into one spreads it over its leaves, by the method chosen under **Spread** in the top bar:
  proportional keeps the leaves' proportions (or shares equally where they are all empty),
  equal gives each leaf the same share, and repeat gives each leaf the value itself.
- **Fill visible leaves** writes random numbers to every numeric leaf on screen in one batch.
- **Begin** opens a transaction. Until you **Commit**, your writes are visible only on this
  page; open the lab in a second tab to see that others do not see them. **Roll back**
  discards them. A transaction left idle for five minutes is rolled back by the server.

## MDX

The grid always shows the answer to the query in the MDX box, and the two drive each other:

- Dragging a dimension, folding a hierarchy or choosing a filter rewrites the query and runs it.
- Typing a query runs it a moment after you stop, or at once with **Run** or Ctrl+Enter.
  While the server rejects it, the box is outlined in red with the reason beside it, and the
  grid keeps what it showed. Once the server accepts it, the grid shows its answer and the
  dimensions, filters and folds move to match it.
- A typed query can show things the controls cannot, such as calculated members, filtered
  sets or a different cube. The next change made with the controls rewrites the query from
  the standard tree of each dimension, keeping unfolded whatever your query showed.
- **View** lists the queries saved with the cube in its model file; choosing one loads and
  runs it. The lab's model also defines subsets on Period, which a query can use with
  `TM1SubsetToSet([Period], 'Months')`.
- **Pretty print** switches between one line and a laid-out query, without running it.
- **Copy** puts the query on the clipboard.

Cells can be edited whatever query is showing, provided the cell is a stored leaf. Inside a
transaction, queries see its pending writes. Hover a heading for the member's unique name.

## Rules

The General Ledger cube has one rule, in its model file: `Net Margin %` is net income over
revenue, at every level. It appears as a row like any other account, but writing to it is
refused. Edit `model/cubes/general-ledger.json` to try more, for example a leaf rule with a
feeder:

```
"rules":   ["N: [Version].[Budget] = [Version].[Actual] * 1.1"],
"feeders": ["[Version].[Actual] => [Version].[Budget]"]
```

`GET /cubes/General%20Ledger/feeders/check` then lists any calculated cells the feeders miss.

The grid shows where each value comes from:

- Purple cells are decided by a rule. In a cube with feeders they are green when fed, and
  purple with an orange outline when they hold a value that nothing feeds, which means the
  totals above them leave it out. Once a cube has any feeder, every leaf rule in it needs one.
- Right-click any cell for the details: its kind, the rule that decides it with its text,
  what is stored beneath a rule, whether it is fed, and each feeder that feeds it with the
  cell it was fed from.

## Signing in

The lab has no users, so it asks for no sign-in. To try sign-in, add a user and restart:

```
target\release\epiphany-server.exe user lab yourname
```

It asks for a password and stores only a hash of it, in `lab/users.json`, which git ignores.
A role after the name limits what the user may do: `read` only reads, `write` also writes
cells, and `admin`, the default, also changes the model. A cube may be given another role in
its place, `none` included:

``
target\release\epiphany-server.exe user lab yourname read "Commentary=write"
```n
Five wrong passwords running keep a name out for a minute.
The page then asks you to sign in, and **Sign out** appears in the top bar. Delete
`users.json` to go back to no sign-in.

## Data

What you write is kept in `data/` (ignored by git) and is still there after a restart. Delete
the `data` folder to start again from an empty cube.

If you edit the model files by hand, cells that no longer fit the model are dropped at the
next start, with a warning in the server window. Updating the model through the API rewrites
the files in `model/` and keeps the previous version under `data/models/`.
