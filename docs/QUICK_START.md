# Epiphany quick start

A one-page guide for a first-time user. No prior experience with planning or
analytics tools is assumed.

## What Epiphany is

Epiphany is a place to plan and analyze numbers across categories. Think of a
spreadsheet that can have more than two directions at once: instead of just rows
and columns, your data is organized by several categories (like region, time
period, and which measure you are looking at), and totals are always kept correct
as you type.

A set of related numbers is called a **cube**. Each category is a **dimension**
(for example Region), and the values within it (North, South, a Total) are its
**members**.

## 1. Sign in

Open the web app in a browser. On the server's first run it creates an
administrator account and writes a one-time password to
`data/server/admin-password.txt` next to the server. Sign in with `admin` and
that password; you will be asked to change it.

## 2. Open the demo cube

A demo cube named **Sales** is loaded automatically so you can explore right
away. Pick it from the sidebar. It is organized by:

- **Region**: North, South, East, with totals Total and Coastal.
- **Period**: Jan, Feb, Mar, with a Q1 total.
- **Measure**: Actual and Budget, with a Variance.

## 3. Enter a number

Go to **Data**. You will see a grid of cells. Click a cell at a leaf position
(for example North / Jan / Actual) and type a number. Press Enter.

## 4. Watch totals update

The totals recalculate immediately. Change North's Actual and the Total row, the
Coastal group, and the Q1 column all update to match. The Variance measure shows
the difference between Actual and Budget without you computing it.

## 5. Look around

- **Views** let you slice the numbers (choose what goes on rows and columns).
- **Dimensions** shows the categories and their members.
- **Schedules** (and **Flows**) automate loading data on a timetable.

## What's next

- **Create your own cube.** Administrators use **Model** to name a cube and its
  categories. You can also reuse a shared category from the **Dimensions**
  library so several cubes stay in step.
- **Load data automatically.** A **Flow** reads rows (from a pasted CSV or a
  data source) and writes them into the cube; a **Schedule** runs it on a
  timetable.
- **Control access.** Administrators grant each person or group exactly the
  access they need in **Security & audit**; by default a cube is private until
  access is granted.

You never have to write code, formulas, or use Git to enter and review data.
Those tools are there for power users when you want them.
