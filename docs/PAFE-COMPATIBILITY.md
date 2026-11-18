# PAfE compatibility table

What each worksheet function of the adapter takes and returns. The adapter is built and
tested against this table.

**Where the table comes from.** The plan asks for a table of *observed* PAfE behaviour. No
PAfE installation was available when this was written, so the "PAfE" column is recalled from
the documented behaviour of the TM1 worksheet functions, and every row marked *unverified*
still has to be checked against a real PAfE workbook. Where the two differ, the row should be
corrected and the adapter with it.

## Conventions

- Names of cubes, dimensions, elements, attributes and subsets are matched ignoring case and
  spaces.
- An element may be given by its name or by any of its aliases.
- A cube may be written `server:cube`, as PAfE workbooks do. The adapter has one connection,
  so the part before the colon is ignored.
- Indexes count from 1.
- `*KEY_ERR` is the text PAfE shows for a reference that cannot be resolved.
- `#N/A` is shown while a result is on its way, and when there is no connection or no session.

## Cell functions

| Function | Arguments | Returns | PAfE |
| --- | --- | --- | --- |
| `DBRW` | cube, one element per dimension | The cell's value: a number, or text for a string cell | Same |
| `DBR` | as `DBRW` | as `DBRW` | Same value. In PAfE `DBR` is not batched; here both are |
| `DBS` | value (number), cube, elements | The value written | Same, unverified |
| `DBSW` | as `DBS` | as `DBS` | Same. In PAfE only `DBSW` is batched; here both are |
| `DBSS` | value (text), cube, elements | The text written | Same, unverified |

| Case | Result | PAfE |
| --- | --- | --- |
| Empty numeric cell | `0` | Same |
| Empty string cell | empty text | Same |
| Consolidated cell | The consolidated number | Same |
| Cell decided by a rule | The rule's value | Same |
| Unknown cube, unknown element, wrong number of elements | `*KEY_ERR` | Same, unverified |
| An argument that is itself an error | That error | Same, unverified |
| `DBS`/`DBSW` to a consolidated cell | `*KEY_ERR`; nothing is written | Refused. PAfE can spread instead when asked to; unverified |
| `DBS`/`DBSW` to a cell decided by a rule | `*KEY_ERR`; nothing is written | Refused, unverified |
| `DBS`/`DBSW` to a string cell, `DBSS` to a numeric cell | `*KEY_ERR`; nothing is written | Refused, unverified |
| `DBS` with a value that is not a number | `#VALUE!`, from Excel | Unverified |
| The cube is locked by another transaction | `#N/A`; recalculate to try again | No equivalent |
| No connection, or the session has ended | `#N/A`, and the sign-in window opens once | Unverified |

## Dimension functions

| Function | Arguments | Returns | Missing | PAfE |
| --- | --- | --- | --- | --- |
| `DIMNM` | dimension, index, [alias] | The element at the index; with an alias attribute, its alias | Index out of range: empty text | Same |
| `DIMIX` | dimension, element | The element's index | Unknown element: `0` | Same |
| `DIMSIZ` | dimension | The number of elements | | Same |
| `ELLEV` | dimension, element | The element's level; a leaf is `0` | Unknown element: `*KEY_ERR` | Same, unverified |
| `ELPAR` | dimension, element, index | The element's parent at the index | No such parent: empty text | Same |
| `ELCOMP` | dimension, element, index | The element's child at the index | No such child: empty text | Same |
| `ELCOMPN` | dimension, element | The number of children | | Same |
| `ELPARN` | dimension, element | The number of parents | | Same |
| `DBRA` | dimension, element, attribute | The attribute's value, text or number | No value: empty text or `0`; an alias without a value gives the element's name | Same, unverified |
| `ATTRS` | dimension, element, attribute | The attribute's value as text | as `DBRA` | Same, unverified |
| `ATTRN` | dimension, element, attribute | The attribute's value as a number | No value: `0`. An attribute that is not numeric: `*KEY_ERR` | Unverified |
| `SUBNM` | dimension, subset, index or element, [alias] | The subset's element at the index, or the named element if the subset holds it; with an alias attribute, its alias | Not in the subset: empty text | Same, unverified |

An unknown dimension, attribute or subset gives `*KEY_ERR`. An empty subset name means every
element of the dimension, in the dimension's order.

`DIMSIZ`, `ELCOMPN` and `ELPARN` are not in the project's list. They are here because
`DIMNM`, `ELCOMP` and `ELPAR` are of little use without a count to loop to.

## Recalculation

| Behaviour | Adapter | PAfE |
| --- | --- | --- |
| A formula recalculates when one of its arguments changes | Yes; Excel does this | Same |
| Refreshing data without changing any argument | **Refresh** in the Epiphany menu, or Ctrl+Shift+R, recalculates every formula in every open workbook | PAfE refreshes from its ribbon; unverified |
| F9 | Recalculates only what Excel holds to be out of date, so it does not fetch new data | Unverified |
| Reads in one calculation | Collected and sent as one `/cells/read` request | Same for `DBRW` |
| Writes in one calculation | Collected and sent as one `/cells/write` request, before the reads of the same calculation. The writes that can be made are made together or not at all; a write the server refuses gets `*KEY_ERR` and does not stop the others | Unverified |
| A `DBS` formula | Writes each time it is calculated | Same |
| Dimension functions | Answered from a copy of the dimension held by the adapter, fetched once and again after **Refresh** | PAfE also caches metadata; unverified |

## Connection and sign-in

| Behaviour | Adapter |
| --- | --- |
| Where the server is | `http://127.0.0.1:8080` until changed with **Connect** in the Epiphany menu. The address and the user's name are remembered in `%APPDATA%\Epiphany\adapter.json`; the password is never stored |
| A server that asks for no sign-in | Works without one |
| A server with users | The first refused request opens the sign-in window. The session's token is kept in memory only |
| A session that has ended | The next request is refused, and the sign-in window opens again |
| **Disconnect** | Signs out and forgets the token |

## Not replicated

- Typing a value over a `DBRW` cell to write it back. Write with `DBS`, `DBSW` or `DBSS`.
- The element picker that PAfE opens on a double click of a `SUBNM` cell.
- Several servers in one workbook.
- `DBS` to a consolidated cell as a spread. Spreading is available from the API
  (`POST /cells/spread`), not from a worksheet function.
- `VIEW`, `TM1RPTROW` and the other functions of PAfE's reports.
