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
- A cube or dimension is written `environment:name`, as PAfE workbooks do:
  `"production:Sales"` is the cube Sales in the environment called production. Each
  environment is a server with its own session. A name without a colon is looked for in the
  default environment. An environment that is not known gives `*KEY_ERR`.
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

## Choosing an element

| Case | Adapter | PAfE |
| --- | --- | --- |
| A double click on a cell whose formula is one `SUBNM` call, or **Choose element** in the Epiphany menu | A window lists the elements of the formula's subset, shown by the alias if the formula names one. Typing narrows the list. The element chosen is written into the formula by name | PAfE opens its set editor; unverified |
| The dimension, subset or alias given as a reference or a formula | Worked out by Excel first | Unverified |
| A cell whose formula is anything else, or whose subset cannot be listed | The double click edits the cell, as in any worksheet | Unverified |

## Typing over a read

| Case | Adapter | PAfE |
| --- | --- | --- |
| A value typed or pasted over a cell whose formula is one `DBRW` or `DBR` call | The value is written to the cube cell the formula reads, and the formula is put back, showing the new value | Same, unverified |
| Over a consolidated or calculated cell | Nothing is written, the formula is put back, and the status bar says so | PAfE can spread instead; unverified |
| A number over a string cell | Written as text | Unverified |
| Text over a numeric cell | Refused, as above | Unverified |
| The cell cleared | The formula is put back; nothing is written | Unverified |
| Other formulas that read the same data | Show the new value after **Refresh** | Unverified |
| A formula that is more than one read, such as `=DBRW(...)*2` | Is replaced by what was typed, as in any worksheet | Same, unverified |
| A cell typed over without having been selected first, or a change of more than 5,000 cells | As any worksheet: the formula is replaced | No equivalent |

## Recalculation

| Behaviour | Adapter | PAfE |
| --- | --- | --- |
| A formula recalculates when one of its arguments changes | Yes; Excel does this | Same |
| Refreshing data without changing any argument | **Refresh** in the Epiphany menu, or Ctrl+Shift+R, asks the server again for every formula in every open workbook | PAfE refreshes from its ribbon; unverified |
| F9 | Recalculates only what Excel holds to be out of date, so it does not fetch new data | Unverified |
| Reads in one calculation | Collected and sent as one `/cells/read` request | Same for `DBRW` |
| Writes in one calculation | Collected and sent as one `/cells/write` request, before the reads of the same calculation. The writes that can be made are made together or not at all; a write the server refuses gets `*KEY_ERR` and does not stop the others | Unverified |
| A `DBS` formula | Writes each time it is calculated | Same |
| Dimension functions | Answered from a copy of the dimension held by the adapter, fetched once and again after **Refresh** | PAfE also caches metadata; unverified |

## Connection and sign-in

| Behaviour | Adapter |
| --- | --- |
| Environments | To begin with there is one, `local`, at `http://127.0.0.1:8080`. **Connect** in the Epiphany menu asks for an environment's name, its server, a user name and a password; it adds the environment, or changes the one of that name, and makes it the default |
| What is remembered | Each environment's server and user name, and which is the default, in `%APPDATA%\Epiphany\adapter.json`. A password is never stored |
| A server that asks for no sign-in | Works without one |
| A server with users | The first refused request opens the Connect window for that environment. The session's token is kept in memory only |
| A session that has ended | The next request is refused, and the window opens again |
| The window closed without signing in | It is not opened again unasked until **Refresh** or **Connect** |
| **Disconnect** | Signs out of every environment and forgets the tokens |

## Not replicated

- `DBS` to a consolidated cell as a spread. Spreading is available from the API
  (`POST /cells/spread`), not from a worksheet function.
- `VIEW`, `TM1RPTROW` and the other functions of PAfE's reports.
