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
            "description": "In-memory multidimensional OLAP server. Clean modern JSON (not OData). Numeric cell values are decimal STRINGS (never JSON numbers) for exactness (ADR-0008). All paths except /healthz, /api/v1/openapi.json and /api/v1/auth/login require a session (bearer token or session cookie)."
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
            "/api/v1/cubes": { "get": {
                "summary": "List cubes", "security": bearer(),
                "responses": ok("The cubes and their cell counts")
            }},
            "/api/v1/cubes/{cube}": { "get": {
                "summary": "A cube with its dimensions and elements", "security": bearer(),
                "parameters": [cube_param()],
                "responses": ok("The cube detail")
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
            "/api/v1/ws": { "get": {
                "summary": "WebSocket change-notification stream", "security": bearer(),
                "responses": { "101": { "description": "Switching protocols (WebSocket)" } }
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
                "Connection": { "type": "object", "properties": {
                    "name": { "type": "string" },
                    "kind": { "type": "string", "enum": ["command"] },
                    "program": { "type": "string", "description": "The executable (command kind)" },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "format": { "type": "string", "enum": ["csv", "json"] },
                    "json_path": { "type": "string", "description": "Dotted path to the JSON record array" },
                    "timeout_ms": { "type": "integer", "format": "int64" } },
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
                    "required": ["name", "flow"] }
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

fn name_param() -> Value {
    json!({
        "name": "name", "in": "path", "required": true,
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
        "/api/v1/cubes/{cube}/cells/read",
        "/api/v1/cubes/{cube}/cell",
        "/api/v1/cubes/{cube}/cells/batch",
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
        "/api/v1/cubes/{cube}/connections",
        "/api/v1/cubes/{cube}/connections/{name}",
        "/api/v1/ws",
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
