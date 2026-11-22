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
| Show or hide empty rows, swap axes, export, undo and redo, defer updates | The grid's toolbar |
| A menu on a cell: sort, drill, show as a share of the row or column | The cell menu |
| Moving between the visual builder and MDX | An MDX view of the same query, editable |
| One query seen as grid or chart | Grid and chart views |
| KPI cards with sparklines, time presets, readable legends | Dashboards, after the explorer |

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
