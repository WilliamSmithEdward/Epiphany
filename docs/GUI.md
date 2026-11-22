# The GUI

A browser application for working with Epiphany's data and running the server: exploring
cubes in a pivot grid, writing back, building views, editing the model, and administering
users and the server.

## Decisions (2026-11-22)

- **Its own program.** `epiphany-gui` is a separate binary in the workspace. It serves the
  application and passes `/api/*` through to an Epiphany server, which may run on the same
  machine or another. The browser sees one origin, and the OLAP server stays as it is.
- **No build step and no npm.** The front end is plain JavaScript modules, web components
  and CSS, embedded in the binary. There are no front-end dependencies to audit, nothing to
  build, and one file to ship. The pivot grid and the charts are written here.
- **Port 80 by default.** `epiphany-gui [--listen address] [--server url]`. Port 80 needs
  no elevation on Windows; on Linux a port below 1024 needs root or the
  `CAP_NET_BIND_SERVICE` capability, which the project does not require. So when port 80
  is refused the program says why and how to choose another port, rather than asking for
  elevation.

## What the reference designs teach

Seven reference screenshots of OLAP tools, kept outside the repository, were read for
their practices, not their looks.

| Practice | Taken as |
| --- | --- |
| A searchable tree of cubes, dimensions, hierarchies, levels and attributes beside the work area | The field list |
| Rows, Columns and Filters areas, with empty areas that say what to drop there | The layout bar, with every drag also reachable by keyboard and menu |
| Filters shown with their chosen members, as in "Gender [Male]" | Filter chips |
| Expandable members, totals set apart, numbers right-aligned in tabular figures, headers that stay in view | The pivot grid |
| Columns fitted to their content, widths and row height dragged by hand and kept with a view | The grid's geometry, with Reset sizes |
| Choosing cells as in a spreadsheet: ranges, whole rows and columns from their headings, Ctrl to add, Shift to stretch, with count, sum, average, min and max beneath | The grid's selection, which copies as tab-separated text and clears writable cells in one write |
| Copy and paste between the grid and a spreadsheet: a block pasted from the active cell, one value into every cell chosen, numbers read as the reader's locale writes them | Grid paste, in one write within the edit transaction; Copy with headings |
| Opening and closing a hierarchy by member, by level and all at once, from the mouse, a menu or the keys | Twisties, the dimension menu (a level more, every level, down to a named level, none, and totals before or after their members), a heading's menu, and Enter, + , - and * on a heading |
| Searching a tree shows each match where it sits, its ancestors dimmed, the words marked, with a count | One tree module for the set editor, the filter picker and the dimension editor, with the WAI-ARIA tree keys and typing to jump |
| Lists in and out: names copied out, names or aliases pasted in, outlines pasted to build a hierarchy | The set editor (Ctrl C, Ctrl V, Paste) and the dimension editor (an element and all beneath it as an outline) |
| Finding a member in a large grid | The grid's find box: n of m, Enter and Shift Enter, and Enter again to open the grid to one not shown |
| Show or hide empty rows, swap axes, export, undo and redo, defer updates | The grid's toolbar |
| A menu on a cell: sort, drill, show as a share of the row or column | The cell menu |
| Moving between the visual builder and MDX | An MDX view of the same query, editable |
| One query seen as grid or chart | Grid and chart views |
| KPI cards with sparklines, time presets, readable legends | Dashboards: tiles of saved views as key figures with their trend, bar, stacked and line charts, or small grids, each opening in Explore |
| A page's selectors that every widget follows (PAW books, Anaplan pages, Power BI slicers) | Dashboard filters: a member chosen above the tiles, which each tile whose cube has the dimension stands at, through the query's `context` |
| Numbers in the units a reader thinks in: decimals, thousands or millions, negatives in parentheses, zeros as a dash (PAW's Format, Power BI's display units) | A view's number format, set from the toolbar, saved with the view and used by its dashboard tiles |
| Typing into a total spreads it, with TM1's codes (P, S or LS, R or LR, C, and + or ~), but a spread over many cells, or one that empties them, asks first | Typed spreading in the grid, within the edit transaction |
| A cell that takes no value says why when typed into (a rule works it out, the role only reads) | The grid's notice, from the facts the server explains |
| What lies behind a number (Excel's Show Details, PAW drill, OneStream's base members) | Show the leaves beneath: a total's consolidated members replaced by their non-empty leaves; Undo goes back |
| A slice of a view into a workbook that stays live (TM1 Perspectives' slice) | Copy as Excel formulas: the cells chosen as DBRW formulas for the add-in, with their headings |
| Undo and redo of data entry (PAW added it after years of asking) that costs nothing to keep | One history of layout and values: a change of values goes back to a point of the edit transaction, which the server keeps as a log of what each write replaced; Redo makes it again |
| Input cells told apart from calculated ones at a glance (PAW's green, Oracle's white on grey) | Writable cells tinted, with a key in the status line |
| Changes tried out apart, kept for days, before anyone else sees them (PAW and Oracle sandboxes, Pigment scenarios, SAP's private versions) | Named sandboxes, chosen beside Views: each a transaction the server keeps for its owner, the cells it changes tinted and counted, the stored value struck through beside each when compared, published or discarded whole |

What they get wrong is avoided as well: crowded ribbons, low contrast, drag and drop as the
only way to do something, and modal dialogs where a side panel would keep the data in view.

## Design rules

- **Tokens.** Colour, space, type and radius are CSS custom properties, with light and dark
  themes and the system's choice followed by default.
- **Accessible.** WCAG 2.2 AA contrast, a visible focus ring on everything, full keyboard
  use, ARIA grid semantics for the pivot, and motion reduced where the system asks.
- **Numbers read well.** Tabular figures, right-aligned, with formats that follow the
  browser's locale, and negatives and missing values clearly told apart.
- **Fast with large results.** Only the rows and columns in view are drawn, so a grid of
  100,000 cells scrolls as smoothly as one of a hundred.
- **Every state designed.** Loading, empty, error with a retry, and refused for want of
  rights.
- **One command palette** (Ctrl+K) reaching every screen and action.
- **Nothing lost to a session ending.** When the session ends, a sign-in dialog opens over
  the page, and the page, its unsaved changes with it, stays as it was. Signing out asks
  first about changes not saved.

## Security

- The session token never reaches the page's scripts. Signing in through the GUI server
  sets an `HttpOnly`, `SameSite=Strict` cookie holding it, and the GUI server adds the
  token to each request it passes on.
- A request that changes anything must also carry a header the GUI's own scripts set,
  which a page on another site cannot.
- Strict security headers on every response: a content security policy allowing only the
  application's own scripts and styles, no framing, no sniffing, no referrer sent away.
- What the user may see and do is decided by the OLAP server, as it is for every client;
  the GUI only hides what would be refused.

## Server additions the admin screens need

- **Users:** list users and their roles, add or change one, and remove one, for admins,
  taking effect at once rather than after a restart.
- **Status:** version, cubes and cells stored, memory held, log and snapshot sizes, open
  sessions and transactions.
- **Sessions and transactions:** list those open, end a session, roll back a transaction.

Each is added to the OpenAPI contract, with the existing route tests.

## Stages

| Stage | Delivers | Done when |
| --- | --- | --- |
| G1 | `epiphany-gui`: embedded assets, the `/api` pass-through, cookie sessions, security headers, port 80 with a clear refusal | Tests pass for pass-through, sign-in, sign-out and the headers |
| G2 | The shell: design tokens, light and dark, sign-in, navigation, command palette, notices | Usable by keyboard alone; contrast checked |
| G3 | Explore: field list, layout bar, member picker, pivot grid with drilling, empty rows, swap axes, MDX view, CSV export | A query built by hand matches the same MDX run through the API |
| G4 | Writing back in the grid, spreading, transactions with commit and rollback | Edits round-trip through the API and are refused where the server refuses them |
| G5 | Charts of a result, saved views, and dashboards built from views | A saved view reopens as it was |
| G6 | Model: dimensions, cubes, rules, feeders and subsets, saved against the model's version | A change made in the GUI matches one sent to the API |
| G7 | Admin: users, roles, sessions, transactions, server status, with the server routes above | An admin can do every listed task; a non-admin sees none of it |
| G8 | Polish: accessibility audit, large-result performance, browser tests of the main journeys | The journeys pass in the browser, and 100,000 cells scroll smoothly |

## Backlog from the survey of OLAP tools (2026-11-21)

A survey of the main planning and OLAP tools' screens and documentation (IBM Planning
Analytics Workspace and TM1, Oracle EPM and Smart View, SAP Analytics Cloud and Analysis for
Office, Excel and Power BI, Anaplan, Pigment, Workday Adaptive Planning, OneStream, Jedox,
Tableau, Atoti) found these gaps. Already taken up: cell comments in a panel beside the grid, dashboard filters, number formats and
totals placement, writable cells marked and read-only ones explained, typed spreading with a
check before sweeping spreads, drilling to the leaves, DBRW formulas copied out, holds that spreads leave alone, pausing the query while a layout is built, data bars, shades and coloured rules kept with views, earlier versions of the model compared and restored, calculations added from a heading (difference, ratio, % change, share of parent), the largest or smallest N rows by a column, the feeder check offered from Explain, undo and
redo of values, only the rows in view drawn, in the grid and in every tree of members, so
that results and dimensions of any size show whole and scroll smoothly, each cell's history
of changes, who made each and from what to what, beside its comments, the changes across
cubes for administrators, by cube, user and time, each opened in Explore, picklists: a
string element's cells offer its values in a list, marked with the list's arrow, and take no
others, and named sandboxes: changes kept apart, on the server, until they are published,
the cells they change marked in the grid, and the stored value struck through beside each
when compared. Still to do, most valuable first:

| Gap | Seen in | Shape here |
| --- | --- | --- |
| Validation rules | Oracle validation rules, Anaplan | Rules over a cube's cells that warn, or refuse a value, as it is saved |
| Supporting detail beneath a cell | Oracle, OneStream, Adaptive Planning | Lines that add up to the cell's value, kept with it |
| Seeing the model as another user does | Pigment | A read-only "view as" with a banner, recorded in the audit log |

Anti-patterns the tools are criticised for, to avoid: spreads that empty whole years from one
typed zero; suppression that hides rows people must plan into; affordances that look like
nothing (picklists that look like plain cells); state that silently disappears (holds lost on
a sort, undo lost on switching workbooks); undo so costly it hangs the server; and switches
that change what every query means.
