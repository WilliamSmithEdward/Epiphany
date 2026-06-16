//! The published OpenAPI 3.1 description, served (unauthenticated) at
//! `GET /api/v1/openapi.json`. Hand-authored for M2 and kept honest by a
//! route-coverage test. Numeric cell values are decimal strings, never JSON
//! numbers (ADR-0008).

use axum::Json;
use serde_json::{json, Value};

pub(crate) async fn openapi_json() -> Json<Value> {
    Json(document())
}

fn bearer() -> Value {
    json!([{ "bearerAuth": [] }])
}

fn json_body(schema_ref: &str) -> Value {
    json!({
        "required": true,
        "content": { "application/json": { "schema": { "$ref": schema_ref } } }
    })
}

fn ok(description: &str) -> Value {
    json!({ "200": { "description": description } })
}

fn document() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Epiphany API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "In-memory multidimensional OLAP server. Clean modern JSON (not OData). Numeric cell values are decimal STRINGS (never JSON numbers) for exactness (ADR-0008). All paths except /healthz, /api/v1/openapi.json and /api/v1/auth/login require a session (bearer token or session cookie). The cell read, view-execute, cellset, explain, and write endpoints accept an optional X-Epiphany-Sandbox header naming a what-if sandbox to overlay (ADR-0014); absent it, they operate on base data."
        },
        "servers": [{ "url": "/" }],
        "paths": {
            "/healthz": { "get": {
                "summary": "Liveness probe", "security": [],
                "responses": ok("Service status and version")
            }},
            "/api/v1/openapi.json": { "get": {
                "summary": "This OpenAPI document", "security": [],
                "responses": ok("The OpenAPI 3.1 document")
            }},
            "/api/v1/auth/login": { "post": {
                "summary": "Log in and receive a session token", "security": [],
                "requestBody": json_body("#/components/schemas/LoginRequest"),
                "responses": {
                    "200": { "description": "A session token and user info", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/LoginResponse" } } } },
                    "401": { "description": "Invalid credentials", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                }
            }},
            "/api/v1/auth/logout": { "post": {
                "summary": "Revoke the current session", "security": bearer(),
                "responses": { "204": { "description": "Logged out" } }
            }},
            "/api/v1/auth/me": { "get": {
                "summary": "The current principal", "security": bearer(),
                "responses": ok("The authenticated user")
            }},
            "/api/v1/auth/password": { "post": {
                "summary": "Change the current user's password", "security": bearer(),
                "requestBody": json_body("#/components/schemas/ChangePasswordRequest"),
                "responses": { "204": { "description": "Password changed" } }
            }},
            "/api/v1/cubes": {
                "get": {
                    "summary": "List cubes", "security": bearer(),
                    "responses": ok("The cubes and their cell counts")
                },
                "post": {
                    "summary": "Create a cube with its dimensions and initial members (admin; ADR-0021)",
                    "security": bearer(),
                    "requestBody": json_body("#/components/schemas/NewCubeRequest"),
                    "responses": {
                        "200": { "description": "The new cube's commit version" },
                        "409": { "description": "A cube with that name already exists", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } },
                        "422": { "description": "Invalid structure (bad name, duplicate dimension, cycle, ...)", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                }
            },
            "/api/v1/cubes/{cube}": { "get": {
                "summary": "A cube with its dimensions and elements", "security": bearer(),
                "parameters": [cube_param()],
                "responses": ok("The cube detail")
            }},
            "/api/v1/dimensions": {
                "get": {
                    "summary": "List the shared dimension library (ADR-0024)", "security": bearer(),
                    "responses": ok("The registered shared dimensions")
                },
                "post": {
                    "summary": "Register a reusable shared dimension (needs global Dimension Write; ADR-0024)",
                    "security": bearer(),
                    "requestBody": json_body("#/components/schemas/NewSharedDimensionRequest"),
                    "responses": {
                        "200": { "description": "The new dimension's id and generation" },
                        "422": { "description": "Invalid structure (bad name, kind conflict, cycle, ...)", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                }
            },
            "/api/v1/dimensions/{id}": {
                "get": {
                    "summary": "A shared dimension's full definition", "security": bearer(),
                    "parameters": [id_param()],
                    "responses": ok("The shared dimension")
                },
                "delete": {
                    "summary": "Delete an unreferenced shared dimension (ADR-0024)", "security": bearer(),
                    "parameters": [id_param()],
                    "responses": {
                        "200": { "description": "Deleted" },
                        "409": { "description": "Still referenced by one or more cubes", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                }
            },
            "/api/v1/dimensions/{id}/elements": { "post": {
                "summary": "Append members/edges to a shared dimension; fans out to every referencing cube (ADR-0024)",
                "security": bearer(),
                "parameters": [id_param()],
                "requestBody": json_body("#/components/schemas/GrowDimensionRequest"),
                "responses": {
                    "200": { "description": "The new generation and the cubes the change fanned out to" },
                    "422": { "description": "Kind conflict, non-consolidated parent, or cycle", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                }
            }},
            "/api/v1/cubes/{cube}/elements": { "post": {
                "summary": "Add elements and consolidation edges to existing dimensions (ADR-0021)",
                "security": bearer(),
                "parameters": [cube_param()],
                "requestBody": json_body("#/components/schemas/AddElementsRequest"),
                "responses": {
                    "200": { "description": "The commit version and number of new elements" },
                    "422": { "description": "Unknown dimension, kind conflict, non-consolidated parent, or cycle", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                }
            }},
            "/api/v1/cubes/{cube}/dimensions/{dim}/attributes/{attr}": { "put": {
                "summary": "Define an attribute on a dimension (text, numeric, or alias; ADR-0021)",
                "security": bearer(),
                "parameters": [cube_param(), dim_param(), attr_param()],
                "requestBody": json_body("#/components/schemas/AttributeRequest"),
                "responses": {
                    "200": { "description": "The commit version" },
                    "422": { "description": "Unknown dimension or a kind conflict", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                }
            }},
            "/api/v1/cubes/{cube}/dimensions/{dim}/attributes/{attr}/values": { "put": {
                "summary": "Set an attribute's value for one or more elements (ADR-0021)",
                "security": bearer(),
                "parameters": [cube_param(), dim_param(), attr_param()],
                "requestBody": json_body("#/components/schemas/AttributeValuesRequest"),
                "responses": {
                    "200": { "description": "The commit version" },
                    "422": { "description": "Unknown element, kind mismatch, or alias collision", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                }
            }},
            "/api/v1/cubes/{cube}/cells/read": { "post": {
                "summary": "Read cell values (consolidation-aware)", "security": bearer(),
                "parameters": [cube_param()],
                "requestBody": json_body("#/components/schemas/ReadCellsRequest"),
                "responses": ok("The requested cells")
            }},
            "/api/v1/cubes/{cube}/cell": { "put": {
                "summary": "Write one leaf cell", "security": bearer(),
                "parameters": [cube_param()],
                "requestBody": json_body("#/components/schemas/WriteCellRequest"),
                "responses": ok("The updated cell")
            }},
            "/api/v1/cubes/{cube}/cells/batch": { "post": {
                "summary": "Apply a transactional batch of writes (all-or-nothing)", "security": bearer(),
                "parameters": [cube_param()],
                "requestBody": json_body("#/components/schemas/BatchWriteRequest"),
                "responses": {
                    "200": { "description": "The batch was applied" },
                    "409": { "description": "Stale base version", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } },
                    "422": { "description": "A write was rejected; nothing applied", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                }
            }},
            "/api/v1/cubes/{cube}/dimensions/{dim}/subsets": {
                "get": {
                    "summary": "List the visible subsets of a dimension", "security": bearer(),
                    "parameters": [cube_param(), dim_param()],
                    "responses": ok("The subsets")
                },
                "post": {
                    "summary": "Create a subset", "security": bearer(),
                    "parameters": [cube_param(), dim_param()],
                    "requestBody": json_body("#/components/schemas/SubsetBody"),
                    "responses": { "201": { "description": "The created subset" } }
                }
            },
            "/api/v1/cubes/{cube}/dimensions/{dim}/subsets/preview": { "post": {
                "summary": "Resolve an unsaved subset to its members", "security": bearer(),
                "parameters": [cube_param(), dim_param()],
                "requestBody": json_body("#/components/schemas/SubsetBody"),
                "responses": ok("The resolved members")
            }},
            "/api/v1/cubes/{cube}/dimensions/{dim}/mdx/preview": { "post": {
                "summary": "Resolve an MDX set expression to members", "security": bearer(),
                "parameters": [cube_param(), dim_param()],
                "requestBody": json_body("#/components/schemas/MdxPreviewRequest"),
                "responses": ok("The resolved members")
            }},
            "/api/v1/cubes/{cube}/dimensions/{dim}/subsets/{name}": {
                "get": {
                    "summary": "A subset", "security": bearer(),
                    "parameters": [cube_param(), dim_param(), name_param()],
                    "responses": ok("The subset")
                },
                "put": {
                    "summary": "Replace a subset", "security": bearer(),
                    "parameters": [cube_param(), dim_param(), name_param()],
                    "requestBody": json_body("#/components/schemas/SubsetBody"),
                    "responses": ok("The updated subset")
                },
                "delete": {
                    "summary": "Delete a subset", "security": bearer(),
                    "parameters": [cube_param(), dim_param(), name_param()],
                    "responses": { "204": { "description": "Deleted" } }
                }
            },
            "/api/v1/cubes/{cube}/dimensions/{dim}/subsets/{name}/members": { "get": {
                "summary": "The resolved members of a saved subset", "security": bearer(),
                "parameters": [cube_param(), dim_param(), name_param()],
                "responses": ok("The resolved members")
            }},
            "/api/v1/cubes/{cube}/views": {
                "get": {
                    "summary": "List the visible views of a cube", "security": bearer(),
                    "parameters": [cube_param()],
                    "responses": ok("The views")
                },
                "post": {
                    "summary": "Create a view", "security": bearer(),
                    "parameters": [cube_param()],
                    "requestBody": json_body("#/components/schemas/ViewBody"),
                    "responses": { "201": { "description": "The created view" } }
                }
            },
            "/api/v1/cubes/{cube}/views/{name}": {
                "get": {
                    "summary": "A view", "security": bearer(),
                    "parameters": [cube_param(), name_param()],
                    "responses": ok("The view")
                },
                "put": {
                    "summary": "Replace a view", "security": bearer(),
                    "parameters": [cube_param(), name_param()],
                    "requestBody": json_body("#/components/schemas/ViewBody"),
                    "responses": ok("The updated view")
                },
                "delete": {
                    "summary": "Delete a view", "security": bearer(),
                    "parameters": [cube_param(), name_param()],
                    "responses": { "204": { "description": "Deleted" } }
                }
            },
            "/api/v1/cubes/{cube}/views/{name}/execute": { "post": {
                "summary": "Execute a saved view to a cellset", "security": bearer(),
                "parameters": [cube_param(), name_param()],
                "responses": ok("The cellset")
            }},
            "/api/v1/cubes/{cube}/cellset": { "post": {
                "summary": "Execute an ad-hoc view spec to a cellset", "security": bearer(),
                "parameters": [cube_param()],
                "requestBody": json_body("#/components/schemas/ViewBody"),
                "responses": ok("The cellset")
            }},
            "/api/v1/cubes/{cube}/rules": {
                "get": {
                    "summary": "The cube's rule source", "security": bearer(),
                    "parameters": [cube_param()], "responses": ok("The rule source")
                },
                "put": {
                    "summary": "Validate and set the cube's rules", "security": bearer(),
                    "parameters": [cube_param()],
                    "requestBody": json_body("#/components/schemas/Rules"),
                    "responses": {
                        "200": { "description": "The stored rules" },
                        "422": { "description": "A rule parse/compile error (with line/column)", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                },
                "delete": {
                    "summary": "Clear the cube's rules", "security": bearer(),
                    "parameters": [cube_param()], "responses": { "204": { "description": "Cleared" } }
                }
            },
            "/api/v1/cubes/{cube}/rules/preview": { "post": {
                "summary": "Validate a rule source without saving", "security": bearer(),
                "parameters": [cube_param()],
                "requestBody": json_body("#/components/schemas/Rules"),
                "responses": ok("Validation result")
            }},
            "/api/v1/cubes/{cube}/cells/explain": { "post": {
                "summary": "A provenance trace for a calculated cell", "security": bearer(),
                "parameters": [cube_param()],
                "requestBody": json_body("#/components/schemas/ExplainRequest"),
                "responses": ok("The provenance trace")
            }},
            "/api/v1/cubes/{cube}/feeders/diagnostics": { "get": {
                "summary": "Auto-inferred feeders and under/over-feed diagnostics", "security": bearer(),
                "parameters": [cube_param()], "responses": ok("The feeder report")
            }},
            "/api/v1/cubes/{cube}/rules/tests": {
                "get": {
                    "summary": "The cube's rule unit tests", "security": bearer(),
                    "parameters": [cube_param()], "responses": ok("The rule tests")
                },
                "post": {
                    "summary": "Create or replace a rule unit test", "security": bearer(),
                    "parameters": [cube_param()],
                    "requestBody": json_body("#/components/schemas/RuleTest"),
                    "responses": { "201": { "description": "The created test" } }
                }
            },
            "/api/v1/cubes/{cube}/rules/tests/run": { "post": {
                "summary": "Run the cube's rule unit tests", "security": bearer(),
                "parameters": [cube_param()], "responses": ok("The test report")
            }},
            "/api/v1/cubes/{cube}/rules/tests/{name}": { "delete": {
                "summary": "Delete a rule unit test", "security": bearer(),
                "parameters": [cube_param(), name_param()],
                "responses": { "204": { "description": "Deleted" } }
            }},
            "/api/v1/cubes/{cube}/flows": { "get": {
                "summary": "The cube's flows", "security": bearer(),
                "parameters": [cube_param()], "responses": ok("The flows")
            }},
            "/api/v1/cubes/{cube}/flows/preview": { "post": {
                "summary": "Validate a flow source without saving", "security": bearer(),
                "parameters": [cube_param()],
                "requestBody": json_body("#/components/schemas/FlowPreview"),
                "responses": {
                    "200": { "description": "Valid" },
                    "422": { "description": "A strip/parse error (with line/column)", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                }
            }},
            "/api/v1/cubes/{cube}/flows/import": { "post": {
                "summary": "Guided CSV import (build members and load values)", "security": bearer(),
                "parameters": [cube_param()],
                "requestBody": json_body("#/components/schemas/FlowImport"),
                "responses": ok("The run report")
            }},
            "/api/v1/cubes/{cube}/flows/tests": {
                "get": {
                    "summary": "The cube's flow tests", "security": bearer(),
                    "parameters": [cube_param()], "responses": ok("The flow tests")
                },
                "post": {
                    "summary": "Create or replace a flow test", "security": bearer(),
                    "parameters": [cube_param()],
                    "requestBody": json_body("#/components/schemas/FlowTest"),
                    "responses": { "201": { "description": "The created test" } }
                }
            },
            "/api/v1/cubes/{cube}/flows/tests/run": { "post": {
                "summary": "Run the cube's flow tests", "security": bearer(),
                "parameters": [cube_param()], "responses": ok("The test report")
            }},
            "/api/v1/cubes/{cube}/flows/tests/{name}": { "delete": {
                "summary": "Delete a flow test", "security": bearer(),
                "parameters": [cube_param(), name_param()],
                "responses": { "204": { "description": "Deleted" } }
            }},
            "/api/v1/cubes/{cube}/flows/{name}": {
                "get": {
                    "summary": "One flow", "security": bearer(),
                    "parameters": [cube_param(), name_param()], "responses": ok("The flow")
                },
                "put": {
                    "summary": "Validate and store a flow", "security": bearer(),
                    "parameters": [cube_param(), name_param()],
                    "requestBody": json_body("#/components/schemas/FlowBody"),
                    "responses": {
                        "200": { "description": "The stored flow" },
                        "422": { "description": "A strip/parse error (with line/column)", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                },
                "delete": {
                    "summary": "Delete a flow", "security": bearer(),
                    "parameters": [cube_param(), name_param()],
                    "responses": { "204": { "description": "Deleted" } }
                }
            },
            "/api/v1/cubes/{cube}/flows/{name}/run": { "post": {
                "summary": "Run a stored flow over uploaded data or a connection", "security": bearer(),
                "parameters": [cube_param(), name_param()],
                "requestBody": json_body("#/components/schemas/FlowRun"),
                "responses": ok("The run report")
            }},
            "/api/v1/cubes/{cube}/jobs": { "get": {
                "summary": "The cube's scheduled jobs (ADR-0013)", "security": bearer(),
                "parameters": [cube_param()], "responses": ok("The jobs")
            }},
            "/api/v1/cubes/{cube}/jobs/{name}": {
                "get": {
                    "summary": "One job", "security": bearer(),
                    "parameters": [cube_param(), name_param()], "responses": ok("The job")
                },
                "put": {
                    "summary": "Create or replace a job (each step must be an existing flow)",
                    "security": bearer(),
                    "parameters": [cube_param(), name_param()],
                    "requestBody": json_body("#/components/schemas/Job"),
                    "responses": {
                        "200": { "description": "The stored job" },
                        "422": { "description": "A step names an unknown flow", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                },
                "delete": {
                    "summary": "Delete a job", "security": bearer(),
                    "parameters": [cube_param(), name_param()],
                    "responses": { "204": { "description": "Deleted" } }
                }
            },
            "/api/v1/cubes/{cube}/jobs/{name}/run": { "post": {
                "summary": "Run a job now (manual kick), returning the run record",
                "security": bearer(),
                "parameters": [cube_param(), name_param()],
                "responses": ok("The run record")
            }},
            "/api/v1/cubes/{cube}/runs": { "get": {
                "summary": "Recent runs for the cube (newest first)", "security": bearer(),
                "parameters": [cube_param()], "responses": ok("The runs")
            }},
            "/api/v1/cubes/{cube}/runs/{id}": { "get": {
                "summary": "One run by id", "security": bearer(),
                "parameters": [cube_param(), id_param()], "responses": ok("The run record")
            }},
            "/api/v1/cubes/{cube}/connections": { "get": {
                "summary": "The cube's data-source connections", "security": bearer(),
                "parameters": [cube_param()], "responses": ok("The connections")
            }},
            "/api/v1/cubes/{cube}/connections/{name}": {
                "get": {
                    "summary": "One connection", "security": bearer(),
                    "parameters": [cube_param(), name_param()], "responses": ok("The connection")
                },
                "put": {
                    "summary": "Define a connection (admin; command kind requires server opt-in)",
                    "security": bearer(),
                    "parameters": [cube_param(), name_param()],
                    "requestBody": json_body("#/components/schemas/Connection"),
                    "responses": {
                        "200": { "description": "The stored connection" },
                        "403": { "description": "Not an admin, or command connectors disabled", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                },
                "delete": {
                    "summary": "Delete a connection (admin)", "security": bearer(),
                    "parameters": [cube_param(), name_param()],
                    "responses": { "204": { "description": "Deleted" } }
                }
            },
            "/api/v1/cubes/{cube}/sandboxes": {
                "get": {
                    "summary": "The caller's what-if sandboxes (admin sees all)", "security": bearer(),
                    "parameters": [cube_param()], "responses": ok("The sandboxes")
                },
                "post": {
                    "summary": "Create a what-if sandbox owned by the caller", "security": bearer(),
                    "parameters": [cube_param()],
                    "requestBody": json_body("#/components/schemas/SandboxCreate"),
                    "responses": {
                        "201": { "description": "The created sandbox" },
                        "409": { "description": "A sandbox of that name exists", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                }
            },
            "/api/v1/cubes/{cube}/sandboxes/{name}": {
                "get": {
                    "summary": "One sandbox (owner or admin)", "security": bearer(),
                    "parameters": [cube_param(), name_param()], "responses": ok("The sandbox")
                },
                "delete": {
                    "summary": "Discard a sandbox (owner or admin)", "security": bearer(),
                    "parameters": [cube_param(), name_param()],
                    "responses": { "204": { "description": "Discarded" } }
                }
            },
            "/api/v1/cubes/{cube}/sandboxes/{name}/commit": { "post": {
                "summary": "Commit a sandbox's what-if values into base (owner or admin)",
                "security": bearer(),
                "parameters": [cube_param(), name_param()],
                "requestBody": { "required": false, "content": { "application/json": { "schema": { "$ref": "#/components/schemas/SandboxCommit" } } } },
                "responses": {
                    "200": { "description": "The commit outcome (new version and committed cell count)" },
                    "409": { "description": "Base moved past the supplied base_version", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                }
            }},
            "/api/v1/ws": { "get": {
                "summary": "WebSocket change-notification stream", "security": bearer(),
                "responses": { "101": { "description": "Switching protocols (WebSocket)" } }
            }},
            "/api/v1/users": {
                "get": {
                    "summary": "List all users (admin)", "security": bearer(),
                    "responses": ok("The users, their admin flag, and group membership")
                },
                "post": {
                    "summary": "Create a user (admin)", "security": bearer(),
                    "requestBody": json_body("#/components/schemas/CreateUserRequest"),
                    "responses": {
                        "201": { "description": "Created" },
                        "409": { "description": "A user of that name exists", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                }
            },
            "/api/v1/users/{username}": {
                "patch": {
                    "summary": "Update a user's admin flag, groups, or password (admin)",
                    "security": bearer(),
                    "parameters": [username_param()],
                    "requestBody": json_body("#/components/schemas/PatchUserRequest"),
                    "responses": { "204": { "description": "Updated" } }
                },
                "delete": {
                    "summary": "Delete a user (admin)", "security": bearer(),
                    "parameters": [username_param()],
                    "responses": { "204": { "description": "Deleted" } }
                }
            },
            "/api/v1/groups": {
                "get": {
                    "summary": "List all groups (admin)", "security": bearer(),
                    "responses": ok("The group names")
                },
                "post": {
                    "summary": "Create a group (admin)", "security": bearer(),
                    "requestBody": json_body("#/components/schemas/CreateGroupRequest"),
                    "responses": { "201": { "description": "Created" } }
                }
            },
            "/api/v1/groups/{name}": { "delete": {
                "summary": "Delete a group (admin)", "security": bearer(),
                "parameters": [name_param()],
                "responses": { "204": { "description": "Deleted" } }
            }},
            "/api/v1/acl/elements": {
                "get": {
                    "summary": "List all element grants (admin)", "security": bearer(),
                    "responses": ok("The element grants")
                },
                "put": {
                    "summary": "Grant or (with level 'none') revoke element access (admin)",
                    "security": bearer(),
                    "requestBody": json_body("#/components/schemas/ElementGrant"),
                    "responses": { "204": { "description": "Applied" } }
                }
            },
            "/api/v1/acl/grants": {
                "get": {
                    "summary": "List modular per-object-kind grants for users and groups (admin; ADR-0023)",
                    "security": bearer(),
                    "responses": ok("The per-kind grants")
                },
                "put": {
                    "summary": "Set or (level=none) revoke a per-kind grant (admin; ADR-0023)",
                    "security": bearer(),
                    "requestBody": json_body("#/components/schemas/Grant"),
                    "responses": { "204": { "description": "Applied" } }
                }
            },
            "/api/v1/audit": { "get": {
                "summary": "Query the audit log (admin)", "security": bearer(),
                "parameters": [
                    { "name": "actor", "in": "query", "required": false, "schema": { "type": "string" } },
                    { "name": "action", "in": "query", "required": false, "schema": { "type": "string" }, "description": "An action token, e.g. access_denied" },
                    { "name": "target", "in": "query", "required": false, "schema": { "type": "string" } },
                    { "name": "outcome", "in": "query", "required": false, "schema": { "type": "string", "enum": ["allowed", "denied"] } },
                    { "name": "from", "in": "query", "required": false, "schema": { "type": "integer", "format": "int64" }, "description": "Inclusive lower bound on timestamp (millis)" },
                    { "name": "to", "in": "query", "required": false, "schema": { "type": "integer", "format": "int64" }, "description": "Inclusive upper bound on timestamp (millis)" },
                    { "name": "offset", "in": "query", "required": false, "schema": { "type": "integer" } },
                    { "name": "limit", "in": "query", "required": false, "schema": { "type": "integer" } }
                ],
                "responses": ok("The matching audit records")
            }}
        },
        "components": {
            "securitySchemes": { "bearerAuth": { "type": "http", "scheme": "bearer" } },
            "schemas": {
                "Error": { "type": "object", "properties": { "error": { "type": "object", "properties": {
                    "code": { "type": "string" }, "message": { "type": "string" }, "details": {} },
                    "required": ["code", "message"] } }, "required": ["error"] },
                "Coord": { "type": "object", "additionalProperties": { "type": "string" },
                    "description": "Dimension name -> element name, one entry per dimension" },
                "LoginRequest": { "type": "object", "properties": {
                    "username": { "type": "string" }, "password": { "type": "string" } },
                    "required": ["username", "password"] },
                "LoginResponse": { "type": "object", "properties": {
                    "token": { "type": "string" }, "user": { "type": "object" } } },
                "ChangePasswordRequest": { "type": "object", "properties": {
                    "current_password": { "type": "string" }, "new_password": { "type": "string" } },
                    "required": ["current_password", "new_password"] },
                "ReadCellsRequest": { "type": "object", "properties": {
                    "coords": { "type": "array", "items": { "$ref": "#/components/schemas/Coord" } } },
                    "required": ["coords"] },
                "WriteCellRequest": { "type": "object", "properties": {
                    "coord": { "$ref": "#/components/schemas/Coord" },
                    "value": { "type": "string", "description": "Decimal string (numeric) or text (string cell)" } },
                    "required": ["coord", "value"] },
                "BatchWriteRequest": { "type": "object", "properties": {
                    "writes": { "type": "array", "items": { "type": "object", "properties": {
                        "coord": { "$ref": "#/components/schemas/Coord" }, "value": { "type": "string" } },
                        "required": ["coord", "value"] } },
                    "base_version": { "type": "integer", "format": "int64" } },
                    "required": ["writes"] },
                "NewCubeRequest": { "type": "object", "properties": {
                    "name": { "type": "string" },
                    "dimensions": { "type": "array", "items": { "type": "object", "properties": {
                        "name": { "type": "string" },
                        "ref": { "type": "integer", "format": "int64", "description": "Reference a registered shared dimension by id; when set, name/elements/edges are ignored and a copy is materialized (ADR-0024)" },
                        "elements": { "type": "array", "items": { "type": "object", "properties": {
                            "name": { "type": "string" },
                            "kind": { "type": "string", "enum": ["numeric", "string", "consolidated"] } },
                            "required": ["name", "kind"] } },
                        "edges": { "type": "array", "items": { "type": "object", "properties": {
                            "parent": { "type": "string" }, "child": { "type": "string" },
                            "weight": { "type": "integer", "format": "int64" } },
                            "required": ["parent", "child"] } } },
                        "required": ["name"] } } },
                    "required": ["name", "dimensions"] },
                "NewSharedDimensionRequest": { "type": "object", "properties": {
                    "name": { "type": "string" },
                    "elements": { "type": "array", "items": { "type": "object", "properties": {
                        "name": { "type": "string" },
                        "kind": { "type": "string", "enum": ["numeric", "string", "consolidated"] } },
                        "required": ["name", "kind"] } },
                    "edges": { "type": "array", "items": { "type": "object", "properties": {
                        "parent": { "type": "string" }, "child": { "type": "string" },
                        "weight": { "type": "integer", "format": "int64" } },
                        "required": ["parent", "child"] } } },
                    "required": ["name"] },
                "GrowDimensionRequest": { "type": "object", "properties": {
                    "elements": { "type": "array", "items": { "type": "object", "properties": {
                        "name": { "type": "string" },
                        "kind": { "type": "string", "enum": ["numeric", "string", "consolidated"] } },
                        "required": ["name", "kind"] } },
                    "edges": { "type": "array", "items": { "type": "object", "properties": {
                        "parent": { "type": "string" }, "child": { "type": "string" },
                        "weight": { "type": "integer", "format": "int64" } },
                        "required": ["parent", "child"] } } } },
                "AddElementsRequest": { "type": "object", "properties": {
                    "elements": { "type": "array", "items": { "type": "object", "properties": {
                        "dimension": { "type": "string" }, "name": { "type": "string" },
                        "kind": { "type": "string", "enum": ["numeric", "string", "consolidated"] } },
                        "required": ["dimension", "name", "kind"] } },
                    "edges": { "type": "array", "items": { "type": "object", "properties": {
                        "dimension": { "type": "string" }, "parent": { "type": "string" },
                        "child": { "type": "string" }, "weight": { "type": "integer", "format": "int64" } },
                        "required": ["dimension", "parent", "child"] } } } },
                "AttributeRequest": { "type": "object", "properties": {
                    "kind": { "type": "string", "enum": ["text", "numeric", "alias"] } },
                    "required": ["kind"] },
                "AttributeValuesRequest": { "type": "object", "properties": {
                    "values": { "type": "array", "items": { "type": "object", "properties": {
                        "element": { "type": "string" }, "value": { "type": "string" } },
                        "required": ["element", "value"] } } },
                    "required": ["values"] },
                "SubsetBody": { "type": "object", "properties": {
                    "name": { "type": "string", "description": "Required to create; ignored on replace/preview" },
                    "visibility": { "type": "string", "enum": ["public", "private"] },
                    "kind": { "type": "string", "enum": ["static", "dynamic"] },
                    "members": { "type": "array", "items": { "type": "string" }, "description": "Static subset members (author order)" },
                    "mdx": { "type": "string", "description": "Dynamic subset MDX set expression" } },
                    "required": ["kind"] },
                "MdxPreviewRequest": { "type": "object", "properties": {
                    "mdx": { "type": "string" } }, "required": ["mdx"] },
                "AxisSpec": { "type": "object", "properties": {
                    "dimension": { "type": "string" },
                    "type": { "type": "string", "enum": ["subset", "members"] },
                    "subset": { "type": "string" },
                    "members": { "type": "array", "items": { "type": "string" } } },
                    "required": ["dimension", "type"] },
                "ViewBody": { "type": "object", "properties": {
                    "name": { "type": "string" },
                    "visibility": { "type": "string", "enum": ["public", "private"] },
                    "suppress_zeros": { "type": "boolean" },
                    "rows": { "type": "array", "items": { "$ref": "#/components/schemas/AxisSpec" } },
                    "columns": { "type": "array", "items": { "$ref": "#/components/schemas/AxisSpec" } },
                    "context": { "type": "array", "items": { "type": "object", "properties": {
                        "dimension": { "type": "string" }, "member": { "type": "string" } },
                        "required": ["dimension", "member"] } } } },
                "Rules": { "type": "object", "properties": {
                    "source": { "type": "string", "description": "Rules-language source text" } },
                    "required": ["source"] },
                "ExplainRequest": { "type": "object", "properties": {
                    "coord": { "$ref": "#/components/schemas/Coord" },
                    "depth": { "type": "string", "description": "immediate | full | a level count" } },
                    "required": ["coord"] },
                "TestCell": { "type": "object", "properties": {
                    "coord": { "$ref": "#/components/schemas/Coord" }, "value": { "type": "string" } },
                    "required": ["coord", "value"] },
                "RuleTest": { "type": "object", "properties": {
                    "name": { "type": "string" },
                    "fixtures": { "type": "array", "items": { "$ref": "#/components/schemas/TestCell" } },
                    "assertions": { "type": "array", "items": { "$ref": "#/components/schemas/TestCell" } } },
                    "required": ["name"] },
                "FlowBody": { "type": "object", "properties": {
                    "name": { "type": "string" },
                    "source": { "type": "string", "description": "TypeScript flow source" } },
                    "required": ["source"] },
                "FlowPreview": { "type": "object", "properties": {
                    "source": { "type": "string" } }, "required": ["source"] },
                "FlowRun": { "type": "object", "properties": {
                    "input": { "type": "string", "description": "Inline data-source content (CSV text)" },
                    "connection": { "type": "string", "description": "A configured connection to fetch rows from, instead of inline input" },
                    "params": { "type": "object", "additionalProperties": { "type": "string" } } } },
                "Job": { "type": "object", "properties": {
                    "name": { "type": "string" },
                    "steps": { "type": "array", "items": { "type": "string" }, "description": "Flow names run in order, fail-fast" },
                    "every_millis": { "type": "integer", "format": "int64", "description": "Interval trigger period in milliseconds" },
                    "enabled": { "type": "boolean" } },
                    "required": ["every_millis", "enabled"] },
                "Connection": { "type": "object", "properties": {
                    "name": { "type": "string" },
                    "kind": { "type": "string", "enum": ["command"] },
                    "program": { "type": "string", "description": "The executable (command kind)" },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "format": { "type": "string", "enum": ["csv", "json"] },
                    "json_path": { "type": "string", "description": "Dotted path to the JSON record array" },
                    "timeout_ms": { "type": "integer", "format": "int64" },
                    "working_dir": { "type": "string", "description": "Absolute working directory (no '..'); the program runs here (ADR-0012 addendum)" } },
                    "required": ["name", "kind"] },
                "FlowImport": { "type": "object", "properties": {
                    "csv": { "type": "string" },
                    "columns": { "type": "object", "additionalProperties": { "type": "string" },
                        "description": "CSV column -> dimension name" },
                    "value_column": { "type": "string" },
                    "fixed": { "type": "object", "additionalProperties": { "type": "string" },
                        "description": "Dimension -> fixed member for unmapped dimensions" } },
                    "required": ["csv", "columns", "value_column"] },
                "FlowTest": { "type": "object", "properties": {
                    "name": { "type": "string" },
                    "flow": { "type": "string" },
                    "input": { "type": "string" },
                    "params": { "type": "object", "additionalProperties": { "type": "string" } },
                    "assertions": { "type": "array", "items": { "$ref": "#/components/schemas/TestCell" } } },
                    "required": ["name", "flow"] },
                "SandboxCreate": { "type": "object", "properties": {
                    "name": { "type": "string", "description": "Unique sandbox name within the cube" } },
                    "required": ["name"] },
                "SandboxCommit": { "type": "object", "properties": {
                    "base_version": { "type": "integer", "format": "int64", "description": "Optimistic base version; omit for last-writer-wins" } } },
                "CreateUserRequest": { "type": "object", "properties": {
                    "username": { "type": "string" },
                    "password": { "type": "string" },
                    "is_admin": { "type": "boolean" },
                    "groups": { "type": "array", "items": { "type": "string" } } },
                    "required": ["username", "password"] },
                "PatchUserRequest": { "type": "object", "properties": {
                    "is_admin": { "type": "boolean" },
                    "groups": { "type": "array", "items": { "type": "string" } },
                    "password": { "type": "string", "description": "Reset the user's password" } } },
                "CreateGroupRequest": { "type": "object", "properties": {
                    "name": { "type": "string" } }, "required": ["name"] },
                "ElementGrant": { "type": "object", "properties": {
                    "cube": { "type": "string" },
                    "dimension": { "type": "string" },
                    "element": { "type": "string" },
                    "subject_kind": { "type": "string", "enum": ["user", "group"] },
                    "subject": { "type": "string" },
                    "level": { "type": "string", "enum": ["none", "read", "write", "admin"], "description": "'none' revokes the grant" } },
                    "required": ["cube", "dimension", "element", "subject_kind", "subject", "level"] },
                "Grant": { "type": "object", "properties": {
                    "subject_kind": { "type": "string", "enum": ["user", "group"] },
                    "subject": { "type": "string" },
                    "scope": { "type": "string", "enum": ["global", "cube"] },
                    "cube": { "type": "string", "description": "Required when scope = cube" },
                    "kind": { "type": "string", "enum": ["cube", "dimension", "rule", "flow", "view", "subset", "job", "connection", "sandbox"] },
                    "level": { "type": "string", "enum": ["none", "read", "write", "admin"], "description": "'none' revokes the grant (ADR-0023)" } },
                    "required": ["subject_kind", "subject", "scope", "kind", "level"] }
            }
        }
    })
}

fn cube_param() -> Value {
    json!({
        "name": "cube", "in": "path", "required": true,
        "schema": { "type": "string" }
    })
}

fn dim_param() -> Value {
    json!({
        "name": "dim", "in": "path", "required": true,
        "schema": { "type": "string" }
    })
}

fn attr_param() -> Value {
    json!({
        "name": "attr", "in": "path", "required": true,
        "schema": { "type": "string" }
    })
}

fn name_param() -> Value {
    json!({
        "name": "name", "in": "path", "required": true,
        "schema": { "type": "string" }
    })
}

fn username_param() -> Value {
    json!({
        "name": "username", "in": "path", "required": true,
        "schema": { "type": "string" }
    })
}

fn id_param() -> Value {
    json!({
        "name": "id", "in": "path", "required": true,
        "schema": { "type": "string" }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every path the document must advertise; mirrors the router.
    const DOCUMENTED_PATHS: &[&str] = &[
        "/healthz",
        "/api/v1/openapi.json",
        "/api/v1/auth/login",
        "/api/v1/auth/logout",
        "/api/v1/auth/me",
        "/api/v1/auth/password",
        "/api/v1/cubes",
        "/api/v1/cubes/{cube}",
        "/api/v1/dimensions",
        "/api/v1/dimensions/{id}",
        "/api/v1/dimensions/{id}/elements",
        "/api/v1/cubes/{cube}/cells/read",
        "/api/v1/cubes/{cube}/cell",
        "/api/v1/cubes/{cube}/cells/batch",
        "/api/v1/cubes/{cube}/elements",
        "/api/v1/cubes/{cube}/dimensions/{dim}/attributes/{attr}",
        "/api/v1/cubes/{cube}/dimensions/{dim}/attributes/{attr}/values",
        "/api/v1/cubes/{cube}/dimensions/{dim}/subsets",
        "/api/v1/cubes/{cube}/dimensions/{dim}/subsets/preview",
        "/api/v1/cubes/{cube}/dimensions/{dim}/mdx/preview",
        "/api/v1/cubes/{cube}/dimensions/{dim}/subsets/{name}",
        "/api/v1/cubes/{cube}/dimensions/{dim}/subsets/{name}/members",
        "/api/v1/cubes/{cube}/views",
        "/api/v1/cubes/{cube}/views/{name}",
        "/api/v1/cubes/{cube}/views/{name}/execute",
        "/api/v1/cubes/{cube}/cellset",
        "/api/v1/cubes/{cube}/rules",
        "/api/v1/cubes/{cube}/rules/preview",
        "/api/v1/cubes/{cube}/cells/explain",
        "/api/v1/cubes/{cube}/feeders/diagnostics",
        "/api/v1/cubes/{cube}/rules/tests",
        "/api/v1/cubes/{cube}/rules/tests/run",
        "/api/v1/cubes/{cube}/rules/tests/{name}",
        "/api/v1/cubes/{cube}/flows",
        "/api/v1/cubes/{cube}/flows/preview",
        "/api/v1/cubes/{cube}/flows/import",
        "/api/v1/cubes/{cube}/flows/tests",
        "/api/v1/cubes/{cube}/flows/tests/run",
        "/api/v1/cubes/{cube}/flows/tests/{name}",
        "/api/v1/cubes/{cube}/flows/{name}",
        "/api/v1/cubes/{cube}/flows/{name}/run",
        "/api/v1/cubes/{cube}/jobs",
        "/api/v1/cubes/{cube}/jobs/{name}",
        "/api/v1/cubes/{cube}/jobs/{name}/run",
        "/api/v1/cubes/{cube}/runs",
        "/api/v1/cubes/{cube}/runs/{id}",
        "/api/v1/cubes/{cube}/connections",
        "/api/v1/cubes/{cube}/connections/{name}",
        "/api/v1/cubes/{cube}/sandboxes",
        "/api/v1/cubes/{cube}/sandboxes/{name}",
        "/api/v1/cubes/{cube}/sandboxes/{name}/commit",
        "/api/v1/ws",
        "/api/v1/users",
        "/api/v1/users/{username}",
        "/api/v1/groups",
        "/api/v1/groups/{name}",
        "/api/v1/acl/elements",
        "/api/v1/acl/grants",
        "/api/v1/audit",
    ];

    #[test]
    fn document_paths_match_the_declared_list() {
        let doc = document();
        let mut paths: Vec<&str> = doc["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        paths.sort_unstable();
        let mut declared = DOCUMENTED_PATHS.to_vec();
        declared.sort_unstable();
        assert_eq!(paths, declared);
    }
}
