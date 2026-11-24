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
| Opening and closing a hierarchy by member, by level and all at once, from the mouse, a menu or the keys | Twisties, the dimension menu (a level more, every level, down to a named level, none, and totals before or after their members), a heading's menu, and Enter, + , - and * on a heading. A member opens where it is opened: shown twice, as chosen and beneath a total, it opens in one place alone. Down the rows each member stands a step in for each member opened above it, and a line runs down from each opened member through the rows beneath it |
| Searching a tree shows each match where it sits, its ancestors dimmed, the words marked, with a count | One tree module for the set editor, the filter picker and the dimension editor, with the WAI-ARIA tree keys and typing to jump |
| Lists in and out: names copied out, names or aliases pasted in, outlines pasted to build a hierarchy | The set editor (Ctrl C, Ctrl V, Paste) and the dimension editor (an element and all beneath it as an outline) |
| Finding a member in a large grid | The grid's find box: n of m, Enter and Shift Enter, and Enter again to open the grid to one not shown |
| Show or hide empty rows, swap axes, export, undo and redo, defer updates | The grid's toolbar |
| A menu on a cell: sort, drill, show as a share of the row or column | The cell menu |
| Moving between the visual builder and MDX | An MDX view of the same query, editable |
| One query seen as grid or chart | Grid and chart views |
| KPI cards with sparklines, time presets, readable legends | Dashboards: tiles of saved views as key figures with their change from the value before and their trend, coloured as the first of the view's highlight rules they meet; bar, stacked, ranked, line, area and donut charts filling the tile, values on the marks if asked, a series for each row or each column; tables of every row with the view's highlights; heatmaps; and notes of text. Each opens in Explore, or over the whole page |
| A page's selectors that every widget follows (PAW books, Anaplan pages, Power BI slicers) | Dashboard filters: a member chosen above the tiles, which each tile whose cube has the dimension stands at, through the query's `context`; a tile whose rows or columns are of the dimension keeps only those at the member or beneath it |
| Arranging a page's visuals by dragging, on a grid or freely, snapped or lined up (Power BI's snap to grid and smart guides, Tableau's tiled and floating objects, a phone's home screen) | A dashboard's board of twelve columns and rows. A tile is dragged by any part of it and resized by its edges or its corner. On the grid the others change places with it as it passes them and rise into what is free, a ghost showing where it will settle; laid out freely it stands where it is let go, over another if put there, on whole columns and rows when snapping or else lined up with an edge or middle it comes near. Sizes at once from its menu, the arrow keys to move it and with Shift to resize it, Undo and Redo, and views dragged in from a list beside |
| Clicking one visual to filter the others (Power BI's cross-filtering, Tableau's filter actions) | A bar, point, slice or row of one tile picked shows every other tile at its members, the picked one marked and said above the tiles, until it is picked again or Escape |
| A page that keeps itself current | Its tiles run again by themselves, every minute to every hour, while the page is in sight |
| A visual pinned to a dashboard from where it was made (Power BI's Pin to dashboard) | Explore's Views offers Pin to a dashboard: the query shown, saved as a view for everyone, and a tile showing it, as the chart shown or a grid, added to a dashboard named or a new one; the notice opens it |
| A page begun from another (Power BI's Duplicate page, Tableau's Duplicate) | New offers a blank dashboard or a copy of the one shown, named "Copy of" it and opened to be arranged before it is saved |
| The numbers behind a visual, seen and taken away (Power BI's Show as a table and Export data) | A tile's menu shows its numbers in place of its chart and back, copies them to paste into a spreadsheet, downloads them as CSV, and opens its view in Explore |
| A dashboard on a wall, or shown to a room (Power BI's full screen, Tableau's presentation mode) | Present: the dashboard alone over the whole screen, or the window where the browser allows no more; the arrows or its buttons go from one dashboard to the next, by itself every 30 seconds if asked, and Escape ends it |
| Numbers in the units a reader thinks in: decimals, thousands or millions, negatives in parentheses, zeros as a dash (PAW's Format, Power BI's display units) | A view's number format, set from the toolbar, saved with the view and used by its dashboard tiles; a calculated member's `FORMAT_STRING`, such as a percentage among sums of money, for the cells it decides |
| Typing into a total spreads it, with TM1's codes (P, S or LS, R or LR, C, + or ~, and P% for a percentage change), but a spread over many cells, or one that empties them, asks first | Typed spreading in the grid, within the edit transaction |
| Filling a plan across months from one cell: a value repeated, shared out, grown by a percentage or run in a straight line (TM1's R>, S>, P>, GR> and SL>) | The same codes with a direction typed into any cell: > to the end of its row, < to its start, ^ up its column and \| or _ down it; leaves are written and totals spread, held cells passed over, as one change to take back |
| A cell that takes no value says why when typed into (a rule works it out, the role only reads) | The grid's notice, from the facts the server explains |
| What lies behind a number (Excel's Show Details, PAW drill, OneStream's base members) | Show the leaves beneath: a total's consolidated members replaced by their non-empty leaves; Undo goes back |
| A slice of a view into a workbook that stays live (TM1 Perspectives' slice) | Copy as Excel formulas: the cells chosen as DBRW formulas for the add-in, with their headings |
| Undo and redo of data entry (PAW added it after years of asking) that costs nothing to keep | One history of layout and values: a change of values goes back to a point of the edit transaction, which the server keeps as a log of what each write replaced; Redo makes it again |
| Input cells told apart from calculated ones at a glance (PAW's green, Oracle's white on grey) | Writable cells tinted, with a key in the status line |
| Changes tried out apart, kept for days, before anyone else sees them (PAW and Oracle sandboxes, Pigment scenarios, SAP's private versions) | Named sandboxes, chosen beside Views: each a transaction the server keeps for its owner, the cells it changes tinted and counted, the stored value struck through beside each when compared, published or discarded whole |
| Views and subsets of one's own, kept apart from those shared with everyone (TM1's and PAW's private views and subsets) | Your views, above everyone's in Explore's Views menu, and your subsets, kept from the set editor's Keep menu and offered beside the dimension's to replace a set with: kept by anyone who signs in, readers too, and seen by nobody else. An administrator saving a view chooses which it is |
| Keeping a user to part of a dimension: regions they may not see, accounts they may only read (TM1's element security) | Roles on elements beside each user's roles on cubes in Administration, each element chosen from its dimension's tree: a hidden element is gone from every page, query and list for them, and a read-only one is not offered for writing |
| Seeing the application as one of its users does, to answer "why can't I…" (Pigment's view as) | View as, beside each user in Administration: the whole application as they have it, reading only, the top bar saying so with a way back, every view kept on record in the audit |
| Keys for programs with no more rights than they need, and still quick to make (GitHub's fine-grained tokens, Stripe's restricted keys) | API keys, in the user menu: each part of the server, cells, the model and administration, a row of No access, Read and Read and write, with Reading only and Everything you may do a click away; kept to particular cubes or not; lasting 30, 90 or 365 days or until revoked; shown once, with Copy; listed with when each was last used, Never for one not used yet. In Administration, May make API keys beside each user's role, off until ticked and always on for administrators, and each user's keys listed to revoke |
| A number broken down into the lines it is made of, kept with it (Oracle's supporting detail, OneStream's and Adaptive Planning's line items) | A cell's Detail, beside its comments and history: lines of a label and an amount whose total the cell takes, saved, undone and discarded with the other changes; a cell with detail underlined with dots, and asked about before a value typed in replaces it |
| Values checked as they are entered, the wrong ones refused or flagged (Oracle's validation rules, Anaplan's data validation) | A cube's checks, made in the model's Checks tab: one that refuses turns the value away with its message and leaves the keys on the cell; one that warns keeps it, says so, and underlines it, and every total it puts wrong, in the grid |

What they get wrong is avoided as well: crowded ribbons, low contrast, drag and drop as the
only way to do something, and modal dialogs where a side panel would keep the data in view.

## Design rules

- **Tokens.** Colour, space, type and radius are CSS custom properties, with light and dark
  themes and the system's choice followed by default. The theme is set by a small script in
  the page's head before anything is drawn, so that a dark page never shows light first.
- **Accessible.** WCAG 2.2 AA contrast, a visible focus ring on everything, full keyboard
  use, ARIA grid semantics for the pivot, the heading of the column its rows are sorted by
  saying the order, with an arrow to the eye, rows of tabs and of choices that are one stop for
  Tab and gone along by the arrow keys, and, where the system asks for less motion, nothing
  that moves or fades. A screen reader hears a heading on every page and a name for every
  control, told apart from the control beside it, and, moving from page to page within the
  application, the page moved to, as on loading one, while the focus stays where it was. A row
  taken away by its button leaves the focus in its list, on the like button of the row in its
  place, or of the one before, or on the list's Add.
- **Numbers read well.** Tabular figures, right-aligned, with formats that follow the
  browser's locale, and negatives and missing values clearly told apart. Moments, as when a
  comment or a change was made, read as long ago as they were, "5 minutes ago", within the
  week, kept up to date as time passes; the moment itself is shown on hover and on paper.
- **Fast with large results.** Only the rows and columns in view are drawn, so a grid of
  100,000 cells scrolls as smoothly as one of a hundred. Pages ask for the model in outline,
  without its elements; a dimension is fetched whole the first time a page shows it, and again
  only once it has changed, as the outline says, its copy before let go: saving a dashboard
  or a view fetches none of them again. On a
  release build that takes about 0.7 s for half a million elements and 2 s for a million,
  fetched, read and made ready (2026-11-22), and 1 s for a million once the server wrote a
  dimension straight from its model (2026-11-23). Members fetched only as a tree is opened
  would take that time away, but every page now reads a dimension as held whole. The
  model's editor opens a dimension of a million elements in 1.8 s, where it took 5.8, and
  shows an element chosen or found in it in 0.15 s, where it took 1.2 and 1.5: its tree, its
  widest name and each element's level are worked out once for each change rather than at
  every click, the widest name reckoned from the widths of its characters, and a search
  climbs from what it finds to the members above (2026-11-23). The modules the
  application imports are fetched beside it rather than once it has come, and the other
  pages' while the browser is idle once the first page shows, so that going to one waits on
  no files. What the GUI's server sends, its files and the API's answers, goes gzipped to a
  browser that takes it, at gzip's quickest: a dimension of 100,000 elements, 10.8 MB of
  JSON, comes to 0.56 MB, made in some 8 ms; the files, 541 KB, to about 160 KB.
- **Fits a phone.** At a phone's width the bar keeps its marks, the words beside them still
  heard by a screen reader; headings and their buttons stack, as do a dialog's fields set side
  by side, an element role taking two lines; and the explorer's buttons go
  along one row that scrolls sideways, as do the chips of each of its layout's zones, beside
  the zone's name, so that the grid shows on the first screen with the most room; a cell's
  comments, history and detail open over the whole screen, there being no room beside the
  grid. No page is wider than the screen, nor any dialog. A rule, of the highlights or of the set editor's
  filter, takes two lines where its box is too narrow for one, its fields still lined up with
  the other rules': on a phone, and in a pane of the set editor on any screen. Under 900
  pixels the set editor's panes go one above the other, each at its own height, its dialog
  scrolling; and a filter too tall for its pane scrolls within it, leaving the tree its room.
  Under 1,100 pixels the model's page shows its list of dimensions and cubes or the one open,
  not both, the one open leading back to the list. A dashboard narrower than 600 pixels
  stands its tiles one above the other, in the order they are read, each its own height; and
  prints the same way.
- **Printable.** A page prints as it reads, without the bar, the list of pages or what only
  controls it: in the light palette whatever the screen's, at its full length, the grid's
  headings repeated on every sheet and its shading, highlights and chart colours kept. A grid
  of up to 200,000 cells and a list of up to 10,000 rows are drawn whole for paper; past that,
  the printout says that only the rows in view are on it.
- **Every state designed.** Loading, empty, error with a retry, and refused for want of
  rights. Moving to another page leaves the one shown, still, until the next is drawn: a
  quick load shows no gap, and a slow one the outline of what is coming. Choosing another
  dimension, cube or dashboard within a page changes only what it shows. A query written by
  hand that the server cannot read offers to show where: the character chosen in the query,
  and what was wrong beneath it, until it is changed. A dialog that
  refuses what a field holds says why beneath it, marks it, invalid to a screen reader and
  described by the refusal, and gives it the keys, until it is changed. A password typed can
  be shown, and hidden again, by the button inside its field, which says beneath it, while
  it is typed in, that Caps Lock is on.
- **Notices that can be read.** A notice stands as long as its words take to read, an error
  twice as long, and waits while it is pointed at, has the keys or the page is out of sight.
  The same notice again takes the place of the first, and four at most stand at once.
- **One command palette** (Ctrl+K) reaching every screen and action: pages, cubes, saved
  views, dashboards and, for administrators, each dimension and cube of the model. The best
  matches come first, the words typed marked in them, and, with nothing typed, the commands
  chosen lately.
- **Nothing lost to a session ending.** When the session ends, a sign-in dialog opens over
  the page, and the page, its unsaved changes with it, stays as it was. Signing out asks
  first about changes not saved, as do closing the page and loading it afresh.
- **Tabs told apart.** The browser's title names what the page shows before the page itself,
  as "General Ledger · Explore · Epiphany", marked with • while it holds changes not saved.
- **Saved by the keys.** Ctrl+S (⌘S on a Mac) saves the changes a page holds, values in
  Explore, a dimension or cube in the model's editor, a dashboard being edited, as its Save
  button does; with nothing to save it says so, rather than offering to save the page as a
  file.
- **Shared by a link.** The Views menu copies a link to the query shown, with its number
  format, highlights and sizes, which opens it as it was shown for anyone who may read the
  cube, each seeing only what they may read. What a link carries is held to what a view may
  say, and a damaged link says so. Shown, the view is the cube's last layout, and the address
  the cube's again. Copying, a link or anything else, works on a page not served securely
  too, as one over plain HTTP from another machine, which browsers give no clipboard.
- **Nothing cut short for good.** A name an ellipsis cuts short shows whole on hover, given
  as a title only while it is cut, and a button shown as an icon alone says on hover the name
  a screen reader hears; neither takes the place of a title of its own.
- **Back to where one was.** The overview lists the six places visited lately, newest first,
  each with when; a page opened at a saved view is named by it. Explore and Dashboards, not
  told which cube or dashboard, open the one shown last, and the address names it.

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
- **Status:** version, cubes and cells stored, memory held, what is kept on disk (the log, the
  snapshot, the model and every earlier model, each said apart), open sessions and
  transactions.
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

Every stage is done; the journeys below are how G8's are kept done.

## Checked in a browser

`crates/epiphany-gui/tests/browser/journeys.mjs` takes the main journeys in Chrome, as a person
takes them, against the builds in `target/debug`, on a copy of `sample/model` with users made for
the run.

| Journey | What it shows |
| --- | --- |
| Signing in | A wrong password refused; the overview; a session that ends signed into again over the page, which stays as it was, and what failed meanwhile fetched again by itself |
| Exploring and writing back | An MDX query typed in shows what the API answers to it; a value typed into a cell is saved and read back through the API; the tabs over the grid gone along by the arrow keys; a cell's comments, history and detail |
| A dashboard | Three tiles of a saved view, a key figure, bars and a grid, run and audited; when they were run said, and Refresh runs them again; the bars' legend, on one line, gone along by its arrows to its last series. The grid's ways of making room checked; a tile dragged to another's place, made taller by its corner, wider by the keys and back with Ctrl Z; laid out freely, let go between columns and lined up with an edge; a tile of text dragged in from the list, and the arrangement saved; a bar picked to show the others at its members; a tile over the whole page; a tile's numbers shown in place of its bars, copied, and the bars again; a key figure in the colour of its view's rule; presented, on to the next dashboard by the arrow, and Escape; copied from New, saved with Quarter's tiles, and deleted; a query pinned from Explore to a new dashboard, opened from the notice, and deleted |
| Changing the model | An element added in the editor is the one the API gives back; a consolidation's children listed to weigh or take out, the keys staying on the list, and the change discarded; an element found by a word of its name |
| Administering | The users; an element hidden from a reader, chosen from its dimension's tree; viewing the server as that reader, who is shown no administration, not the element, and whose writes are refused; back to the administrator, the view kept in the audit |
| API keys | A key made from the user menu, given all its user may do with one click, shown once and never again, taken by the server, and listed as used just now; a reader let make keys, who may make one that reads and not one that writes; the reader's key revoked from Administration and refused at once; the administrator's revoked from their own list |
| A large grid | 100,000 cells: only those in view drawn, and scrolled to the end at the pace the page keeps at rest; printed, all of them drawn, and the 2,000 elements of its rows' dimension too |

At each step `audit.js` checks the page as it stands, in the light theme and the dark: a
control without a name, a field named only by its placeholder, a tab order of its own, the keys
reaching what a screen reader is told is not there, an image without words, an id given twice,
a page without one main landmark or a heading, a row of tabs or choices that is more than one
stop for Tab, anything that moves when motion is to be reduced, and text that stands out from
what is behind it by less than WCAG AA asks.

```
cargo build -p epiphany-server -p epiphany-gui
node crates/epiphany-gui/tests/browser/journeys.mjs
```

It needs Node and Chrome or Edge and installs nothing, speaking to the browser over the
DevTools protocol on a pipe. CI runs it on every push. On Windows, the browser's fresh profile
is given Chrome's answer to whether the user's password is blank: left to find out, Chrome signs
in to Windows with an empty password at every run, and the refused sign-ins lock the account
(and Remote Desktop with it) after ten runs within minutes, as they did on 2026-11-22.

## Backlog from the survey of OLAP tools (2026-11-21)

A survey of the main planning and OLAP tools' screens and documentation (IBM Planning
Analytics Workspace and TM1, Oracle EPM and Smart View, SAP Analytics Cloud and Analysis for
Office, Excel and Power BI, Anaplan, Pigment, Workday Adaptive Planning, OneStream, Jedox,
Tableau, Atoti) found these gaps. Already taken up: cell comments in a panel beside the grid,
dashboard filters, number formats and totals placement, writable cells marked and read-only
ones explained, typed spreading with a check before sweeping spreads, drilling to the leaves,
DBRW formulas copied out, holds that spreads leave alone, pausing the query while a layout is
built, data bars, shades and coloured rules kept with views, earlier versions of the model
compared and restored, calculations added from a heading (difference, ratio, % change, share
of parent), the largest or smallest N rows by a column, the feeder check offered from
Explain, undo and redo of values, only the rows in view drawn, in the grid and in every tree
of members, so
that results and dimensions of any size show whole and scroll smoothly, each cell's history
of changes, who made each and from what to what, beside its comments, the changes across
cubes for administrators, by cube, user and time, each opened in Explore, picklists: a
string element's cells offer its values in a list, marked with the list's arrow, and take no
others, named sandboxes: changes kept apart, on the server, until they are published,
the cells they change marked in the grid, and the stored value struck through beside each
when compared, checks: a cube's rules of what a value may be, which refuse a value as it is
written or keep it, underlined in the grid with what the check says, supporting detail: the
lines a cell's value is the sum of, kept with it, and viewing the server as another user
does, reading only, with the top bar saying so and the audit keeping it on record. Every gap
the survey found is taken up.

Anti-patterns the tools are criticised for, to avoid: spreads that empty whole years from one
typed zero; suppression that hides rows people must plan into; affordances that look like
nothing (picklists that look like plain cells); state that silently disappears (holds lost on
a sort, undo lost on switching workbooks); undo so costly it hangs the server; and switches
that change what every query means.
