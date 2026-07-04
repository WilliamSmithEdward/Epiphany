//! Cube-detail and cell endpoints (name-addressed, clean JSON). Writes funnel
//! through the engine's atomic batch commit; reads are consolidation-aware on a
//! lock-free snapshot.

use std::str::FromStr;

use axum::extract::{Path, State};
use axum::Json;

use epiphany_core::{
    spread_leaves, AttributeKind, AttributeValue, CellResolver, Cube, ElementMask, Fixed,
    SpreadError, SpreadMethod,
};
use epiphany_engine::{BatchError, CellWrite, Engine, ReadSnapshot};
use epiphany_security::AccessLevel;

use crate::auth::AuthPrincipal;
use crate::authz::{
    element_mask, require_cube_access, require_element_write, require_element_write_indices,
};
use crate::calc_factory::RuleCoverage;
use crate::dto::{
    AttributeDto, AttributeValueDto, BatchWriteRequest, BatchWriteResponse, CellDto, CoordMap,
    CubeDetailDto, DimensionDto, EdgeDto, ElementDto, ReadCellsRequest, ReadCellsResponse,
    SpreadRequest, WriteCellRequest,
};
use crate::resolve::{kind_str, resolve, Resolved};
use crate::sandbox_routes::{resolve_sandbox, SandboxSelector};
use crate::ws::ChangeEvent;
use crate::{ApiError, AppState};

// ---- shared route helpers (used across the route modules) ----

/// Pin a lock-free read snapshot of a cube, or 404 if it does not exist
/// (ADR-0001). The single definition shared by every route module.
pub(crate) fn snapshot(state: &AppState, cube: &str) -> Result<ReadSnapshot, ApiError> {
    state
        .engine
        .snapshot(cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))
}

/// Run a blocking engine MUTATION (`op`) off the async worker threads (A2).
///
/// Every engine commit/checkpoint takes the per-cube writer lock and then does
/// blocking disk I/O under it — at least one WAL fsync, and a full-model checkpoint
/// for a definitional or sandbox op. Called inline from an async handler that would
/// OS-block a tokio worker on the mutex + fsync; a handful of concurrent writers (or
/// one slow-disk checkpoint) then starves the runtime, including the "lock-free"
/// snapshot reads and `/healthz`, which still need a worker thread (ADR-0013 already
/// routes flow work this way; the HTTP write surface did not). The engine is
/// `Clone + Send` (a cheap Arc handle), so this clones it into a `spawn_blocking`
/// task and awaits the result, leaving writer-lock contention and fsync latency on
/// the blocking pool. Error mapping and response shapes are unchanged: a
/// `BatchError` still flows through [`map_batch_error`]; a task-join failure (a
/// panic in the op) is a clean 500.
pub(crate) async fn blocking_commit<T, F>(engine: &Engine, op: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Engine) -> Result<T, BatchError> + Send + 'static,
{
    let engine = engine.clone();
    tokio::task::spawn_blocking(move || op(&engine))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(map_batch_error)
}

/// Emit an `ObjectsChanged` for a cube at a specific version, for the FEW mutations
/// that do NOT flow through the engine commit path — so the commit-ordered change
/// feed (A1) never fires for them. Today that is only promoting a cube's dimension
/// into the global registry (a registry-only op with no cube commit). Every
/// committing mutation is emitted by the [`ChangeFeed`](crate::ws::ChangeFeed)
/// observer instead, so this must not be called after a commit (it would double the
/// event and reintroduce post-commit reordering).
pub(crate) fn broadcast_with_version(state: &AppState, cube: &str, version: u64) {
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube: cube.to_string(),
        version,
    });
}

/// `GET /api/v1/cubes/{cube}` -> the cube with its dimensions and elements.
pub(crate) async fn get_cube(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<CubeDetailDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    // Element security (ADR-0015): a non-admin must not learn the names of
    // elements denied to them, so denied members (and any edge touching them) are
    // suppressed from the cube structure, just as from a member enumeration.
    let mask = element_mask(&state, &auth, &snap);
    // The global dimension id (ADR-0024/0031) when a dimension is registry-backed,
    // so the web can present one global dimension list and route edits correctly.
    // Resolved in one registry pass, then a cheap per-dimension map lookup.
    let backings = state.engine.dimension_backings(&cube);
    let backing = |dim_name: &str| backings.get(dim_name).map(|id| id.0);
    Ok(Json(cube_detail(snap.cube(), mask.as_ref(), backing)))
}

fn cube_detail(
    cube: &Cube,
    mask: Option<&ElementMask>,
    backing: impl Fn(&str) -> Option<u64>,
) -> CubeDetailDto {
    let dimensions = cube
        .dimensions()
        .iter()
        .enumerate()
        .map(|(d, dim)| {
            let denied = |idx: u32| mask.is_some_and(|m| m.denies_member(cube, d, idx));
            DimensionDto {
                name: dim.name().to_string(),
                id: backing(dim.name()),
                elements: dim
                    .iter_elements()
                    .enumerate()
                    .filter(|(i, _)| !denied(*i as u32))
                    .map(|(_, el)| ElementDto {
                        name: el.name.clone(),
                        kind: kind_str(el.kind),
                        pinned_to_top: el.pinned_to_top,
                    })
                    .collect(),
                edges: dim
                    .edges()
                    .into_iter()
                    .filter(|(parent, child, _)| !denied(*parent) && !denied(*child))
                    .map(|(parent, child, weight)| EdgeDto {
                        parent: dim.element(parent).expect("valid index").name.clone(),
                        child: dim.element(child).expect("valid index").name.clone(),
                        weight,
                    })
                    .collect(),
                attributes: dimension_attributes(dim, &denied),
            }
        })
        .collect();
    CubeDetailDto {
        name: cube.name().to_string(),
        dimensions,
    }
}

fn attr_kind_str(kind: AttributeKind) -> &'static str {
    match kind {
        AttributeKind::Text => "text",
        AttributeKind::Numeric => "numeric",
        AttributeKind::Alias => "alias",
    }
}

/// Build the attribute DTOs for one dimension, suppressing values whose element
/// the caller may not see (element security).
fn dimension_attributes(
    dim: &epiphany_core::Dimension,
    denied: &impl Fn(u32) -> bool,
) -> Vec<AttributeDto> {
    let defs = dim.attribute_defs();
    let mut per_attr: Vec<Vec<AttributeValueDto>> = (0..defs.len()).map(|_| Vec::new()).collect();
    for (element, attr_index, value) in dim.attribute_values() {
        if denied(element) {
            continue;
        }
        let element_name = dim.element(element).expect("valid index").name.clone();
        let text = match value {
            AttributeValue::Text(t) => t,
            AttributeValue::Numeric(n) => n.to_string(),
        };
        per_attr[attr_index as usize].push(AttributeValueDto {
            element: element_name,
            value: text,
        });
    }
    defs.iter()
        .zip(per_attr)
        .map(|(def, values)| AttributeDto {
            name: def.name.clone(),
            kind: attr_kind_str(def.kind),
            values,
        })
        .collect()
}

/// `POST /api/v1/cubes/{cube}/cells/read` -> values for a set of coordinates
/// (consolidation-aware).
pub(crate) async fn read_cells(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    selector: SandboxSelector,
    Json(req): Json<ReadCellsRequest>,
) -> Result<Json<ReadCellsResponse>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    let cube_ref = snap.cube();
    // An active sandbox (X-Epiphany-Sandbox) overlays its what-if leaves beneath
    // the rules, so values recompute over them (ADR-0014); absent it, base.
    let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
    let sandbox = sandbox_name
        .as_deref()
        .and_then(|n| snap.model().sandbox(n));
    // Values come through the injected resolver (rule-aware in the server,
    // stored-only in no-rules deployments and tests), carrying the caller's
    // element deny mask (ADR-0015): a directly-addressed denied coordinate (or a
    // rollup of a denied leaf) returns 403.
    let mask = element_mask(&state, &auth, &snap);
    let resolver = state.cells.resolver_with(&snap, sandbox, mask.as_ref());
    // Rule coverage drives the `editable` flag (ADR-0040): a rule-calculated leaf
    // renders read-only so the client's write path matches the server's. Built
    // once; a cube with no rules skips the per-cell probe.
    let coverage = RuleCoverage::build(&state.engine, &snap);
    let mut cells = Vec::with_capacity(req.coords.len());
    for coord in &req.coords {
        let resolved = resolve(cube_ref, coord)?;
        // A cell is "overlaid" only when this exact leaf is a what-if override
        // (a consolidation that merely rolled one up is not flagged). Overrides
        // are numeric this phase (ADR-0014), so only `cells` is consulted.
        let overlaid = sandbox.is_some_and(|sb| sb.cells.contains_key(&resolved.indices));
        let covered = resolved.all_leaf
            && coverage.has_rules()
            && coverage.covers(cube_ref, &resolved.indices);
        cells.push(read_one(&*resolver, coord, &resolved, overlaid, covered)?);
    }
    Ok(Json(ReadCellsResponse { cells }))
}

/// `PUT /api/v1/cubes/{cube}/cell` -> write one leaf cell, return its new value.
pub(crate) async fn write_cell(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    selector: SandboxSelector,
    Json(req): Json<WriteCellRequest>,
) -> Result<Json<CellDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    // Element security (ADR-0015): a write to a coordinate the caller may not
    // write is rejected before anything is staged.
    require_element_write(&state, &auth, &cube, &req.coord)?;
    // Resolve the coordinate name -> index and submit with the resolving
    // snapshot's version as the optimistic base, retrying on a concurrent commit.
    // A structural edit (ADR-0036) committed between resolve and apply remaps
    // element indices; without a base an index-addressed write would silently land
    // on the WRONG element. Binding the base makes such a race a conflict we retry
    // by re-resolving the name on the fresh snapshot, so the write always lands on
    // the element the caller named (the external contract stays "succeeds").
    // The commit version is not needed here (the change feed emits it; the response
    // re-reads on a fresh snapshot below), so discard the outcome.
    let (_outcome, sandbox_name, _applied) = commit_resolved(&state, &cube, |snap| {
        let sandbox_name = resolve_sandbox(snap, &auth.principal, &selector)?;
        let writes = vec![build_write(snap.cube(), &req.coord, &req.value)?];
        // A rule-calculated leaf is not writable (ADR-0040): reject rather than
        // silently persist a value the rule will shadow on the next read.
        reject_rule_covered_writes(&state, snap, &writes)?;
        Ok((writes, sandbox_name))
    })
    .await?;
    // The commit-ordered change feed (A1) emits the `CellsChanged` from inside the
    // engine commit (in version order, tagged with the sandbox for owner-scoped
    // delivery), so the handler no longer sends it here — doing so would double the
    // event and reintroduce the post-commit reordering the observer fixes.
    // (`sandbox_name` is still used just below to overlay the re-read.)
    // Re-read on a fresh snapshot, overlaying the sandbox so the caller sees the
    // staged what-if value (flagged overlaid).
    let snap = state
        .engine
        .snapshot(&cube)
        .ok_or_else(ApiError::internal)?;
    let sandbox = sandbox_name
        .as_deref()
        .and_then(|n| snap.model().sandbox(n));
    let mask = element_mask(&state, &auth, &snap);
    let resolver = state.cells.resolver_with(&snap, sandbox, mask.as_ref());
    let resolved = resolve(snap.cube(), &req.coord)?;
    let overlaid = sandbox.is_some_and(|sb| sb.cells.contains_key(&resolved.indices));
    // A just-written leaf is never rule-covered (the write above would have been
    // rejected), but re-derive the flag from the same authority for consistency.
    let coverage = RuleCoverage::build(&state.engine, &snap);
    let covered = resolved.all_leaf && coverage.covers(snap.cube(), &resolved.indices);
    Ok(Json(read_one(
        &*resolver, &req.coord, &resolved, overlaid, covered,
    )?))
}

/// `POST /api/v1/cubes/{cube}/cells/batch` -> apply all writes or none.
pub(crate) async fn batch_write(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    selector: SandboxSelector,
    Json(req): Json<BatchWriteRequest>,
) -> Result<Json<BatchWriteResponse>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    // Element security (ADR-0015): if any write targets a coordinate the caller
    // may not write, the whole batch is rejected before anything is staged.
    for w in &req.writes {
        require_element_write(&state, &auth, &cube, &w.coord)?;
    }
    let (writes, sandbox_name) = {
        let snap = snapshot(&state, &cube)?;
        let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
        let cube_ref = snap.cube();
        let writes = req
            .writes
            .iter()
            .map(|item| build_write(cube_ref, &item.coord, &item.value))
            .collect::<Result<Vec<_>, _>>()?;
        // Reject the whole batch if any target is a rule-calculated leaf
        // (ADR-0040), before anything is staged — consistent with the
        // element-security all-or-nothing gate above.
        reject_rule_covered_writes(&state, &snap, &writes)?;
        (writes, sandbox_name)
    };
    // A what-if batch stages into the sandbox (base untouched). Honor the client's
    // optimistic `base_version` on BOTH paths (the engine's sandbox define supports
    // a base check, like commit_sandbox does): dropping it on the sandbox path would
    // silently give last-writer-wins and let two sessions clobber each other's
    // what-if overrides despite sending a concurrency guard.
    //
    // Run the commit (writer lock + fsync) off the async workers (A2). The change
    // feed (A1) emits the `CellsChanged` from inside the commit; the handler no
    // longer sends it. `sandbox_name` only selects the sandbox-vs-base path here.
    let applied = writes.len();
    let base = req.base_version;
    let cube_name = cube.clone();
    let outcome = blocking_commit(&state.engine, move |engine| match &sandbox_name {
        Some(name) => engine.sandbox_set_cells(&cube_name, base, name, &writes),
        None => engine.apply_batch(&cube_name, base, &writes),
    })
    .await?;
    Ok(Json(BatchWriteResponse {
        applied,
        version: outcome.version,
    }))
}

/// `POST /api/v1/cubes/{cube}/cells/spread` -> distribute a value entered at a
/// (possibly consolidated) coordinate across its leaves (ADR-0029).
pub(crate) async fn spread_cells(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    selector: SandboxSelector,
    Json(req): Json<SpreadRequest>,
) -> Result<Json<BatchWriteResponse>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    let method = parse_spread_method(&req.method)?;
    let value = Fixed::from_str(&req.value).map_err(|_| {
        ApiError::unprocessable("INVALID_NUMBER", format!("invalid number '{}'", req.value))
    })?;

    // Re-expand and commit under the optimistic base, retrying on a concurrent
    // commit (ADR-0036): the expanded leaves are index-addressed, so a structural
    // edit between expand and apply would otherwise silently shift the spread onto
    // different leaves. Binding the resolving snapshot's version as the base turns
    // that race into a conflict we retry by re-expanding on the fresh snapshot;
    // the proportional basis and element-security check use that same snapshot.
    let (outcome, sandbox_name, applied) = commit_resolved(&state, &cube, |snap| {
        let sandbox_name = resolve_sandbox(snap, &auth.principal, &selector)?;
        let resolved = resolve(snap.cube(), &req.target)?;
        if resolved.has_string {
            return Err(ApiError::unprocessable(
                "SPREAD_TO_STRING",
                "cannot spread into a string cell",
            ));
        }
        // Expand the target into leaf writes, reading current values (for the
        // proportional basis) through a resolver that honors the sandbox + mask.
        // Rule-covered leaves (ADR-0040) are excluded before distribution, so the
        // entered total spreads only across leaves that can hold it and reproduces
        // on read-back; if every target leaf is rule-covered the spread is rejected
        // (there is nowhere to place the value).
        let expanded = {
            let sandbox = sandbox_name
                .as_deref()
                .and_then(|n| snap.model().sandbox(n));
            let mask = element_mask(&state, &auth, snap);
            let resolver = state.cells.resolver_with(snap, sandbox, mask.as_ref());
            let coverage = RuleCoverage::build(&state.engine, snap);
            let cube_ref = snap.cube();
            spread_leaves(
                cube_ref,
                &resolved.indices,
                value,
                method,
                &|c| resolver.value(c),
                &|c| coverage.covers(cube_ref, c),
            )
            .map_err(map_spread_error)?
        };
        // Element security (ADR-0015): fail-closed. If any contributing leaf is not
        // writable, the whole spread is denied before anything is staged.
        let coords: Vec<Vec<u32>> = expanded.iter().map(|(c, _)| c.clone()).collect();
        require_element_write_indices(&state, &auth, &cube, snap, &coords)?;
        let batch: Vec<CellWrite> = expanded
            .into_iter()
            .map(|(coord, value)| CellWrite::Leaf { coord, value })
            .collect();
        Ok((batch, sandbox_name))
    })
    .await?;
    // The commit-ordered change feed (A1) emits the `CellsChanged` from inside the
    // engine commit; the handler no longer sends it (see `write_cell`). `sandbox_name`
    // only decided the sandbox-vs-base commit path inside `commit_resolved`.
    let _ = &sandbox_name;
    Ok(Json(BatchWriteResponse {
        applied,
        version: outcome.version,
    }))
}

fn parse_spread_method(token: &str) -> Result<SpreadMethod, ApiError> {
    match token {
        "equal" => Ok(SpreadMethod::Equal),
        "proportional" => Ok(SpreadMethod::Proportional),
        "repeat" => Ok(SpreadMethod::Repeat),
        "clear" => Ok(SpreadMethod::Clear),
        other => Err(ApiError::bad_request(format!(
            "unknown spread method '{other}'"
        ))),
    }
}

fn map_spread_error(err: SpreadError) -> ApiError {
    match err {
        SpreadError::WeightedConsolidation => ApiError::unprocessable(
            "SPREAD_WEIGHTED",
            "cannot spread across a weighted consolidation",
        ),
        SpreadError::TooManyLeaves { count, cap } => ApiError::unprocessable(
            "SPREAD_TOO_LARGE",
            format!("the target expands to {count} cells, over the limit of {cap}"),
        ),
        SpreadError::AllExcluded => ApiError::unprocessable(
            "RULE_COVERED_CELL",
            "every target leaf is rule-calculated; there is nothing to spread into",
        ),
        SpreadError::Read(query_error) => ApiError::from(query_error),
    }
}

fn read_one(
    cells: &dyn CellResolver,
    coord: &CoordMap,
    resolved: &Resolved,
    overlaid: bool,
    covered: bool,
) -> Result<CellDto, ApiError> {
    if resolved.has_string {
        let value = cells.string_value(&resolved.indices)?;
        Ok(CellDto {
            coord: coord.clone(),
            value,
            kind: "string",
            // A string cell is not numerically editable; and a rule-covered leaf
            // is read-only (ADR-0040) so the client never offers an input the
            // write path would reject.
            editable: resolved.all_leaf && !covered,
            overlaid,
        })
    } else {
        let value = cells.value(&resolved.indices)?;
        Ok(CellDto {
            coord: coord.clone(),
            value: Some(value.to_string()),
            kind: "numeric",
            editable: resolved.all_leaf && !covered,
            overlaid,
        })
    }
}

/// How many times a name-resolved write re-resolves and retries when a concurrent
/// commit moves the cube's version out from under it. A structural edit (ADR-0036)
/// is rare and a commit strictly advances the version, so a small bound converges;
/// exhausting it (sustained contention) surfaces the 409 rather than looping.
const RESOLVE_RETRY_LIMIT: usize = 8;

/// Resolve a name-addressed write against a fresh snapshot and commit it with that
/// snapshot's version as the optimistic base, retrying on a `Conflict` by
/// re-resolving on the new snapshot. This closes the versionless-write race: an
/// index-addressed batch submitted with no base could land on the wrong element if
/// a structural edit remapped indices between resolve and apply; binding the base
/// turns that race into a conflict we transparently retry, so the write lands on
/// the element the caller named. `build` returns the writes plus the resolved
/// sandbox name (both are re-derived per attempt from the passed snapshot).
async fn commit_resolved(
    state: &AppState,
    cube: &str,
    build: impl Fn(&ReadSnapshot) -> Result<(Vec<CellWrite>, Option<String>), ApiError>,
) -> Result<(epiphany_engine::CommitOutcome, Option<String>, usize), ApiError> {
    let mut last_conflict: Option<BatchError> = None;
    for _ in 0..RESOLVE_RETRY_LIMIT {
        let snap = snapshot(state, cube)?;
        let base = snap.version();
        let (writes, sandbox_name) = build(&snap)?;
        // Drop the snapshot before committing so we do not pin it across the write.
        drop(snap);
        // An empty batch is a no-op: report the current version without committing
        // (so, e.g., a spread that expands to no writable leaf does not bump the
        // version), matching the pre-existing short-circuit.
        if writes.is_empty() {
            return Ok((
                epiphany_engine::CommitOutcome { version: base },
                sandbox_name,
                0,
            ));
        }
        // Run the commit (writer lock + fsync) off the async workers (A2). The raw
        // `BatchError` is inspected here so a `Conflict` still drives the re-resolve
        // retry; only a terminal error is mapped to an `ApiError`. The engine is a
        // cheap Clone handle, and the owned writes/cube/sandbox move into the task.
        let applied = writes.len();
        let engine = state.engine.clone();
        let cube_owned = cube.to_string();
        let sandbox_for_commit = sandbox_name.clone();
        let result = tokio::task::spawn_blocking(move || match &sandbox_for_commit {
            Some(name) => engine.sandbox_set_cells(&cube_owned, Some(base), name, &writes),
            None => engine.apply_batch(&cube_owned, Some(base), &writes),
        })
        .await
        .map_err(|_| ApiError::internal())?;
        match result {
            Ok(outcome) => return Ok((outcome, sandbox_name, applied)),
            Err(e @ BatchError::Conflict { .. }) => {
                // A concurrent commit advanced the version; re-resolve and retry.
                last_conflict = Some(e);
            }
            Err(e) => return Err(map_batch_error(e)),
        }
    }
    Err(map_batch_error(
        last_conflict.expect("loop ran at least once"),
    ))
}

/// Reject the batch if any write targets a rule-covered leaf (ADR-0040): the rule
/// computes that cell, so a stored value would be silently shadowed on every read
/// (`write_cell` would even return the rule value, not what was sent). Fail-loud
/// with a typed 422 instead. Checked against the same pinned `snap` the write
/// commits against, so it cannot race a concurrent rule change; a cube with no
/// rules is a cheap no-op. Batches are all-or-nothing (like the element-security
/// gate): one covered cell rejects the whole batch before anything is staged.
fn reject_rule_covered_writes(
    state: &AppState,
    snap: &ReadSnapshot,
    writes: &[CellWrite],
) -> Result<(), ApiError> {
    let coverage = RuleCoverage::build(&state.engine, snap);
    if !coverage.has_rules() {
        return Ok(());
    }
    for w in writes {
        if let CellWrite::Leaf { coord, .. } = w {
            if coverage.covers(snap.cube(), coord) {
                return Err(ApiError::unprocessable(
                    "RULE_COVERED_CELL",
                    "cannot write to a rule-calculated cell; its value is computed by a rule",
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn build_write(
    cube: &Cube,
    coord: &CoordMap,
    value: &str,
) -> Result<CellWrite, ApiError> {
    let resolved = resolve(cube, coord)?;
    if !resolved.all_leaf {
        return Err(ApiError::unprocessable(
            "WRITE_TO_NON_LEAF",
            "cannot write to a consolidated coordinate",
        ));
    }
    if resolved.has_string {
        Ok(CellWrite::Str {
            coord: resolved.indices,
            value: value.to_string(),
        })
    } else {
        let number = Fixed::from_str(value).map_err(|_| {
            ApiError::unprocessable("INVALID_NUMBER", format!("invalid number '{value}'"))
        })?;
        Ok(CellWrite::Leaf {
            coord: resolved.indices,
            value: number,
        })
    }
}

/// Map a persistence failure from the global automation store (ADR-0035) into a
/// 500: the cause is never serialized (RG-12), matching the cube write path.
pub(crate) fn map_persist_error(_error: epiphany_persist::PersistError) -> ApiError {
    ApiError::internal()
}

pub(crate) fn map_batch_error(error: BatchError) -> ApiError {
    match error {
        BatchError::UnknownCube(cube) => ApiError::not_found(format!("unknown cube '{cube}'")),
        BatchError::Conflict { expected, actual } => ApiError::conflict(format!(
            "stale base version {expected}; the cube is at {actual}"
        )),
        BatchError::Rejected { index, source } => ApiError::unprocessable(
            "BATCH_REJECTED",
            format!("write {index} rejected: {source}"),
        )
        .with_details(serde_json::json!({ "failed_index": index })),
        // A structurally invalid definition carries a typed QueryError with its
        // own status and code (404 for a missing object, 422 otherwise).
        BatchError::Invalid(e) => ApiError::from(e),
        BatchError::AlreadyExists(name) => {
            ApiError::conflict(format!("cube '{name}' already exists"))
        }
        BatchError::Unsupported(what) => ApiError::unprocessable("UNSUPPORTED", what),
        BatchError::Persist(_) => ApiError::internal(),
    }
}
