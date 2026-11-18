The goal of this project is to create an original in-memory OLAP server built in RUST that is as simple and performant as possible. Keep it simple, DRY, ultra-clean code.



Operating framework: docs/RIDM.md



Requirements:



OpenAPI: Your single API contract.



MDX: Your query language.



JSON: Request and response serialization.



Rust: Entire server and execution engine.



PAfE compatibility adapter: A small integration layer translating existing formula operations into your API in Excel-DNA.





PAfE Compatibility:

Functionality	Purpose

DBRW	Read cube values

DBR	Read cube values

DBS	Numeric writeback

DBSW	Numeric writeback

DBSS	String writeback

SUBNM	Select dimension members

DBRA	Read attributes

DIMNM / DIMIX	Dimension and element lookup

ATTRS / ATTRN	Read string and numeric attributes

ELLEV / ELPAR / ELCOMP	Hierarchy navigation



Also replicate:

\- Formula recalculation behavior.

\- Batched cell reads and writes.

\- Error and missing-value semantics.

\- Member aliases.

\- Connection and authentication handling.





OLAP:

Functionality	Purpose

Cubes	Multidimensional data containers

Dimensions	Coordinate definitions

Hierarchies	Parent-child structures

Elements	Members and identifiers

Consolidations	Aggregated members

Attributes	Metadata associated with elements

Subsets	Reusable member selections

Views	Saved multidimensional queries

Rules	Calculated cells

Feeders	Sparse calculation optimization

Writeback	Update leaf cells

Spreading	Allocate values across intersections





MDX Evaluation:

SELECT, FROM, WHERE.

ROWS and COLUMNS axes.

Members, tuples and sets.

CrossJoin.

Filter.

Order.

Descendants and Ancestors.

Hierarchize.

NonEmpty.

Calculated members.

Named sets.

Properties and member metadata.





Through API (including across multiple rest calls): Needs to support read/write/ix locks, transactions and rollback. Atomic batch writes and multiple ops in a transaction with rollback.





Model Definition (structure, not data storage) in JSON (REST API should support updating model. Needs to be atomic and versioned, etc.):

Example (Not all inclusive, just to give you an idea)--

model/

├── model.json

├── dimensions/

│   ├── account.json

│   ├── entity.json

│   └── period.json

└── cubes/

&#x20;   ├── general-ledger.json

&#x20;   └── workforce.json



Dimensions



{

&#x20; "name": "Account",

&#x20; "type": "standard",

&#x20; "elements": \[

&#x20;   {

&#x20;     "name": "Revenue",

&#x20;     "type": "consolidated",

&#x20;     "children": \[

&#x20;       { "name": "Product Revenue", "weight": 1 },

&#x20;       { "name": "Service Revenue", "weight": 1 }

&#x20;     ]

&#x20;   },

&#x20;   {

&#x20;     "name": "Product Revenue",

&#x20;     "type": "numeric"

&#x20;   },

&#x20;   {

&#x20;     "name": "Service Revenue",

&#x20;     "type": "numeric"

&#x20;   },

&#x20;   {

&#x20;     "name": "Operating Expenses",

&#x20;     "type": "consolidated",

&#x20;     "children": \[

&#x20;       { "name": "Salaries", "weight": 1 },

&#x20;       { "name": "Rent", "weight": 1 }

&#x20;     ]

&#x20;   },

&#x20;   {

&#x20;     "name": "Salaries",

&#x20;     "type": "numeric"

&#x20;   },

&#x20;   {

&#x20;     "name": "Rent",

&#x20;     "type": "numeric"

&#x20;   }

&#x20; ],

&#x20; "attributes": {

&#x20;   "AccountType": "string",

&#x20;   "AccountCode": "string"

&#x20; }

}

Cubes

{

&#x20; "name": "General Ledger",

&#x20; "dimensions": \[

&#x20;   "Account",

&#x20;   "Entity",

&#x20;   "Period",

&#x20;   "Version"

&#x20; ]

}

