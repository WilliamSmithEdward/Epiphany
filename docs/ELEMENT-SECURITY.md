# Permissions by element: the design

`docs/PLAN.md` §3 lists permissions by element as not done. This is the design, with TM1's
element security as the model, and the questions it leaves for the owner.

## Today

A user has a role, `read`, `write` or `admin`, and may have another on a cube: `none`,
`read` or `write`. Each route asks about the cube it names (`allow` in `api.rs`); a query
is refused for the cube it selects from and any it looks cells up in (`may_read`, passed to
`Database::query_as`).

## Proposed

1. **Where it is set.** Per user, beside `cubes`, in `users.json` and in
   `PUT /admin/users/{name}`: `"elements": { "Region": { "Europe": "none", "North America":
   "read" } }`, each role `none`, `read` or `write`. An administrator is not bound by it.
2. **What an element's role is.** Its own, or else that of the nearest element above it that
   has one; an element beneath several such takes the highest of theirs. An element with
   none above it takes the role on its cube. A cell's role is the lowest of its cube's and
   of its elements'.
3. **`none` hides the element**, for that user, everywhere its name would show:
   - `GET /dimensions/{name}` leaves it out, as an element and as a child or parent, and
     out of the members of subsets and the values of picklists; `/model/outline` counts
     without it.
   - MDX does not find it by name or alias, and leaves it out of every set: `.Members`,
     `.Children`, `Descendants`, levels, ranges, subsets, drilling and the rest.
   - A cell at it is refused, as a cell of a cube with `none` is: `/cells/read`,
     `/cells/explain`, `/cells/history`, comments; `/cubes/{cube}/cells` and the feeder
     check leave such cells out.
4. **`read`** lets the element be seen and its cells read, not written.
5. **Totals** are added up over every leaf beneath them, those hidden from the user
   included, as TM1 adds them up: a total is the same number for everyone who may see it,
   and the totals kept for all users stay right.
6. **Writes.** A write to a cell the user may not write is refused, as a write to a cube
   they may only read is. A spread is refused if any leaf it would change is one the user
   may not write; holds aside, nothing is passed over unsaid.
7. **The Excel add-in** answers its dimension functions (`DIMNM`, `DIMIX`, `ELPAR`, `SUBNM`
   and the rest) from the dimension as `GET /dimensions/{name}` gives it, so hidden elements
   are not there for it either; a `DBRW` of a refused cell shows the add-in's refusal.
8. **Viewing as another user** shows what they may see.

## The questions

- **Inheritance**: a role on a consolidation holds beneath it (as above), or each
  element's role is set on it alone (TM1's way, which its administrators fill out by
  script).
- **Texts the model holds**: rules, feeders, the queries of views and the MDX of subsets may
  name elements a user may not see. Shown to such a user as written (they are the model's,
  and hiding them would hide how its numbers come about), or `GET /model` and
  `GET /cubes/{name}` refused to a user with any hidden element.
- **Totals over hidden leaves**: included (as above), or left out, which would make a total
  different from one user to the next and every total worked out for each user afresh.
- **A spread that reaches a leaf the user may not write**: refused (as above), or that leaf
  passed over as a held one is.
