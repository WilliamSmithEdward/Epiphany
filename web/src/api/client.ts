// Typed client for the Epiphany REST API. Numeric cell values are decimal
// STRINGS, never JS numbers (ADR-0008), so they never lose precision. The
// session token is kept in memory (not localStorage); the server also sets an
// HttpOnly cookie, which authenticates the WebSocket.

export type Coord = Record<string, string>
export type ElementKind = 'numeric' | 'string' | 'consolidated'

export interface ElementDto {
  name: string
  kind: ElementKind
  /** Legacy top-level pin flag (ADR-0038) still sent by some servers. The web
   * client no longer honors it: pinning was removed from the UI, so a member is a
   * display root iff it has no parent. Retained on the type only to document the
   * wire. Absent on older servers. */
  pinned_to_top?: boolean
}

export interface EdgeDto {
  parent: string
  child: string
  weight: number
}

export interface DimensionDto {
  name: string
  /** The global dimension id when this cube dimension is backed by the registry
   * (ADR-0024/0031); absent for a cube-embedded-only dimension. Lets the explorer
   * present one global dimension namespace and route edits to the right place. */
  id?: number
  elements: ElementDto[]
  edges: EdgeDto[]
  /** Attributes defined on this dimension (ADR-0021). Absent on older servers. */
  attributes?: AttributeDto[]
}

export interface AttributeValueDto {
  element: string
  value: string
}

export interface AttributeDto {
  name: string
  kind: AttributeKind
  values: AttributeValueDto[]
}

export interface CubeDetail {
  name: string
  dimensions: DimensionDto[]
}

export interface CubeSummary {
  name: string
  rank: number
  cell_count: number
  string_cell_count: number
}

export interface CellDto {
  coord: Coord
  value: string | null
  kind: 'numeric' | 'string'
  editable: boolean
  /** True when the value is a what-if override from the active sandbox. */
  overlaid: boolean
}

export interface LoginResult {
  token: string
  user: { username: string; is_admin: boolean; must_change_password: boolean }
}

/** The current principal, as returned by GET /api/v1/auth/me. Used to restore
 * an in-tab session on reload via the HttpOnly session cookie (the in-memory
 * bearer token is gone after a reload, so the cookie authenticates this call). */
export interface MeResponse {
  username: string
  is_admin: boolean
  must_change_password: boolean
  /** The caller's OWN effective persona (server-derived from their grants,
   * self-only): 'business' | 'modeler' | 'admin'. Drives the shell's progressive
   * disclosure (ADR-0020). Authoritative — do not re-derive on the client. */
  persona: Persona
}

export interface BatchResult {
  applied: number
  version: number
}

let token: string | null = null

export function setToken(value: string | null): void {
  token = value
}

// The active what-if sandbox (ADR-0014). When set, every data request carries
// the X-Epiphany-Sandbox header, so reads recompute over the sandbox and writes
// stage into it; null means base. Managed by the sandbox switcher.
let activeSandbox: string | null = null

export function setActiveSandbox(value: string | null): void {
  activeSandbox = value
}

export function getActiveSandbox(): string | null {
  return activeSandbox
}

/** A failed API call, carrying the HTTP status so callers can distinguish e.g.
 * an authorization denial (403) from a transient server/network failure. A
 * network error before any response has `status` undefined. */
export class ApiError extends Error {
  constructor(
    message: string,
    public readonly status?: number,
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

/** Per-request options threaded through the read path (ADR-0020 performance
 * mandate). `signal` lets a caller abort a superseded / unmounted request so a
 * stale response can never clobber newer state and abandoned server work is
 * freed; a live multi-user tool whose grids refetch on every WebSocket event
 * accumulates such requests otherwise. Write helpers stay signal-free (a commit
 * in flight should complete). */
export interface RequestOptions {
  signal?: AbortSignal
}

/** True for the DOMException a fetch throws when its AbortSignal fires. Callers
 * that abort a superseded request in an effect cleanup use this to swallow the
 * rejection silently (a cancel is not an error the user should see) rather than
 * painting an error banner for a request they themselves cancelled. */
export function isAbortError(err: unknown): boolean {
  return err instanceof DOMException && err.name === 'AbortError'
}

/** The standard request headers: bearer auth (when signed in), the active
 * sandbox, and a JSON content-type when a body is sent. Shared so every request
 * path attaches auth the same way. */
function authHeaders(hasBody: boolean): Record<string, string> {
  const headers: Record<string, string> = {}
  if (token) headers['authorization'] = `Bearer ${token}`
  if (activeSandbox) headers['x-epiphany-sandbox'] = activeSandbox
  if (hasBody) headers['content-type'] = 'application/json'
  return headers
}

// An app-level handler notified once whenever a session-expired 401 is seen, so
// the shell can return to the Login screen immediately rather than leaving a
// zombie UI whose every panel shows a local "session expired" error. Set by
// App.tsx; a module-level slot (not an event target) keeps the client
// framework-free and avoids leaking listeners.
let onSessionExpired: (() => void) | null = null

/** Register (or clear, with null) the app-level session-expired handler. Called
 * on the first session-expired 401 of a dead session so the app can re-show the
 * Login screen in place. */
export function setSessionExpiredHandler(handler: (() => void) | null): void {
  onSessionExpired = handler
}

/** Clear the in-memory token and throw the uniform expired-session error. Called
 * on any 401 so every request path reports session expiry identically; also
 * notifies the app-level handler so the shell can re-show Login in place. */
function throwSessionExpired(): never {
  setToken(null)
  onSessionExpired?.()
  throw new ApiError('Your session has expired. Please sign in again.', 401)
}

// Auth endpoints where a 401 is a credential rejection (wrong password, wrong
// current password), NOT an expired session: mapping those to "session expired"
// would both hide the server's accurate message on the sign-in and
// change-password forms and wrongly clear a still-valid in-memory token. These
// fall through to the normal error-envelope parsing so the server's message
// surfaces (login/changePassword throw a plain ApiError with the real reason).
const AUTH_ENDPOINTS = new Set(['/api/v1/auth/login', '/api/v1/auth/password'])

async function request<T>(
  method: string,
  path: string,
  body?: unknown,
  opts?: RequestOptions,
): Promise<T> {
  const response = await fetch(path, {
    method,
    headers: authHeaders(body !== undefined),
    // Send the same-origin HttpOnly session cookie. This is the browser default
    // for same-origin requests; setting it explicitly keeps cookie-based auth
    // working (e.g. restoring a session on reload, when the in-memory bearer
    // token is gone) and documents that we never want 'omit'.
    credentials: 'same-origin',
    body: body === undefined ? undefined : JSON.stringify(body),
    // When supplied, aborting the signal rejects this fetch with an AbortError
    // (see isAbortError); callers wire it to an AbortController cancelled in an
    // effect cleanup so a superseded read cannot land its response.
    signal: opts?.signal,
  })
  if (response.status === 401 && !AUTH_ENDPOINTS.has(path)) throwSessionExpired()
  if (!response.ok) {
    let message = `Request failed (${response.status})`
    try {
      const parsed = (await response.json()) as { error?: { message?: string } }
      if (parsed.error?.message) message = parsed.error.message
    } catch {
      /* keep the default message */
    }
    throw new ApiError(message, response.status)
  }
  // A successful response may carry no body (204, or a 201 Created with no
  // payload). Only parse JSON when a body is actually present, so a bodyless
  // 2xx does not throw "Unexpected end of JSON input".
  const text = await response.text()
  return (text ? (JSON.parse(text) as T) : (undefined as T))
}

export async function login(username: string, password: string): Promise<LoginResult> {
  const result = await request<LoginResult>('POST', '/api/v1/auth/login', { username, password })
  setToken(result.token)
  return result
}

/**
 * Fetch the current principal. After an in-tab reload the in-memory bearer
 * token is null, so this is authenticated by the HttpOnly session cookie; it is
 * allowed even during a pending forced password change (it is in the server's
 * MUST_CHANGE_ALLOWED set), so the forced-rotation screen can be restored too.
 */
export async function getMe(): Promise<MeResponse> {
  return request<MeResponse>('GET', '/api/v1/auth/me')
}

export async function logout(): Promise<void> {
  try {
    await request<void>('POST', '/api/v1/auth/logout')
  } finally {
    setToken(null)
  }
}

/**
 * Change the current user's password. Allowed even while a forced rotation is
 * pending (the server keeps this session and revokes the user's others), so the
 * caller stays signed in afterwards.
 */
export async function changePassword(
  currentPassword: string,
  newPassword: string,
): Promise<void> {
  await request<void>('POST', '/api/v1/auth/password', {
    current_password: currentPassword,
    new_password: newPassword,
  })
}

export async function listCubes(): Promise<CubeSummary[]> {
  const result = await request<{ cubes: CubeSummary[] }>('GET', '/api/v1/cubes')
  return result.cubes
}

export async function getCube(cube: string, opts?: RequestOptions): Promise<CubeDetail> {
  return request<CubeDetail>('GET', `/api/v1/cubes/${encodeURIComponent(cube)}`, undefined, opts)
}

export async function readCells(
  cube: string,
  coords: Coord[],
  opts?: RequestOptions,
): Promise<CellDto[]> {
  const result = await request<{ cells: CellDto[] }>(
    'POST',
    `/api/v1/cubes/${encodeURIComponent(cube)}/cells/read`,
    { coords },
    opts,
  )
  return result.cells
}

export async function writeCell(cube: string, coord: Coord, value: string): Promise<CellDto> {
  return request<CellDto>('PUT', `/api/v1/cubes/${encodeURIComponent(cube)}/cell`, { coord, value })
}

/** How a spread distributes a value across the contributing leaves (ADR-0029). */
export type SpreadMethod = 'equal' | 'proportional' | 'repeat' | 'clear'

/** Spread a value entered at a (possibly consolidated) coordinate across its leaves. */
export async function spreadCells(
  cube: string,
  target: Coord,
  value: string,
  method: SpreadMethod,
): Promise<{ applied: number; version: number }> {
  return request('POST', `/api/v1/cubes/${encodeURIComponent(cube)}/cells/spread`, {
    target,
    value,
    method,
  })
}

/** Atomically write multiple cells. When `baseVersion` is given it is sent as
 * `base_version`, enabling the server's optimistic-concurrency check: the commit
 * is rejected with 409 if the cube moved on since that version (callers hold it
 * as CellsetDto.version), so a concurrent edit is detected rather than silently
 * last-writer-wins. Omit it to force the write unconditionally. */
export async function batchWrite(
  cube: string,
  writes: { coord: Coord; value: string }[],
  baseVersion?: number,
): Promise<BatchResult> {
  return request<BatchResult>('POST', `/api/v1/cubes/${encodeURIComponent(cube)}/cells/batch`, {
    writes,
    ...(baseVersion !== undefined ? { base_version: baseVersion } : {}),
  })
}

// ---- sandboxes (what-if, ADR-0014) ----

/** A what-if sandbox as returned by the server. */
export interface SandboxDto {
  name: string
  owner: string
  created: number
  updated: number
  cell_count: number
}

function sandboxBase(cube: string): string {
  return `/api/v1/cubes/${encodeURIComponent(cube)}/sandboxes`
}

export async function listSandboxes(cube: string): Promise<SandboxDto[]> {
  const result = await request<{ sandboxes: SandboxDto[] }>('GET', sandboxBase(cube))
  return result.sandboxes
}

export async function createSandbox(cube: string, name: string): Promise<SandboxDto> {
  return request<SandboxDto>('POST', sandboxBase(cube), { name })
}

export async function deleteSandbox(cube: string, name: string): Promise<void> {
  return request<void>('DELETE', `${sandboxBase(cube)}/${encodeURIComponent(name)}`)
}

export async function commitSandbox(
  cube: string,
  name: string,
): Promise<{ version: number; committed: number }> {
  return request<{ version: number; committed: number }>(
    'POST',
    `${sandboxBase(cube)}/${encodeURIComponent(name)}/commit`,
  )
}

// ---- subsets, views, and cellsets (Phase 3) ----

export type Visibility = 'public' | 'private'
export type SubsetKindTag = 'static' | 'dynamic'

/** A subset as returned by the server. */
export interface SubsetDto {
  name: string
  dimension: string
  owner: string | null
  visibility: Visibility
  kind: SubsetKindTag
  members: string[]
  mdx?: string
}

/** A subset definition sent to the server (create, replace, or preview). */
export interface SubsetDef {
  name?: string
  visibility?: Visibility
  kind: SubsetKindTag
  members?: string[]
  mdx?: string
}

export interface MemberDto {
  name: string
  kind: ElementKind
}

/** One axis placement in a view definition. */
export type AxisSpecDef =
  | { dimension: string; type: 'subset'; subset: string }
  | { dimension: string; type: 'members'; members: string[] }

export interface ContextEntry {
  dimension: string
  member: string
}

/** A view definition sent to the server (create, replace, or ad-hoc execute). */
export interface ViewDef {
  name?: string
  visibility?: Visibility
  /** Drop result rows whose values are all zero across the shown columns. */
  suppress_zero_rows?: boolean
  /** Drop result columns whose values are all zero across the shown rows. */
  suppress_zero_columns?: boolean
  rows: AxisSpecDef[]
  columns: AxisSpecDef[]
  context?: ContextEntry[]
}

export interface AxisSpecDto {
  dimension: string
  type: 'subset' | 'members'
  subset?: string
  members?: string[]
}

export interface ViewDto {
  name: string
  cube: string
  owner: string | null
  visibility: Visibility
  suppress_zero_rows: boolean
  suppress_zero_columns: boolean
  rows: AxisSpecDto[]
  columns: AxisSpecDto[]
  context: ContextEntry[]
}

export interface AxisMemberDto {
  dimension: string
  name: string
  kind: ElementKind
}

export interface CellsetCellDto {
  value: string | null
  kind: 'numeric' | 'string'
  editable: boolean
  ordinal: number
  /** True when the value is a what-if override from the active sandbox. */
  overlaid: boolean
}

export interface CellsetDto {
  row_dimensions: string[]
  column_dimensions: string[]
  row_tuples: AxisMemberDto[][]
  column_tuples: AxisMemberDto[][]
  context: ContextEntry[]
  cells: CellsetCellDto[]
  version: number
  suppressed: { row_tuples: number; column_tuples: number }
}

function dimBase(cube: string, dim: string): string {
  return `/api/v1/cubes/${encodeURIComponent(cube)}/dimensions/${encodeURIComponent(dim)}`
}

export async function listSubsets(cube: string, dim: string): Promise<SubsetDto[]> {
  const result = await request<{ subsets: SubsetDto[] }>('GET', `${dimBase(cube, dim)}/subsets`)
  return result.subsets
}

export async function createSubset(cube: string, dim: string, def: SubsetDef): Promise<SubsetDto> {
  return request<SubsetDto>('POST', `${dimBase(cube, dim)}/subsets`, def)
}

export async function updateSubset(
  cube: string,
  dim: string,
  name: string,
  def: SubsetDef,
): Promise<SubsetDto> {
  return request<SubsetDto>('PUT', `${dimBase(cube, dim)}/subsets/${encodeURIComponent(name)}`, def)
}

export async function deleteSubset(cube: string, dim: string, name: string): Promise<void> {
  return request<void>('DELETE', `${dimBase(cube, dim)}/subsets/${encodeURIComponent(name)}`)
}

export async function previewSubset(cube: string, dim: string, def: SubsetDef): Promise<MemberDto[]> {
  const result = await request<{ members: MemberDto[] }>(
    'POST',
    `${dimBase(cube, dim)}/subsets/preview`,
    def,
  )
  return result.members
}

export async function previewMdx(
  cube: string,
  dim: string,
  mdx: string,
  opts?: RequestOptions,
): Promise<MemberDto[]> {
  const result = await request<{ members: MemberDto[] }>(
    'POST',
    `${dimBase(cube, dim)}/mdx/preview`,
    { mdx },
    opts,
  )
  return result.members
}

export async function listViews(cube: string): Promise<ViewDto[]> {
  const result = await request<{ views: ViewDto[] }>(
    'GET',
    `/api/v1/cubes/${encodeURIComponent(cube)}/views`,
  )
  return result.views
}

export async function getView(cube: string, name: string): Promise<ViewDto> {
  return request<ViewDto>('GET', `/api/v1/cubes/${encodeURIComponent(cube)}/views/${encodeURIComponent(name)}`)
}

export async function createView(cube: string, def: ViewDef): Promise<ViewDto> {
  return request<ViewDto>('POST', `/api/v1/cubes/${encodeURIComponent(cube)}/views`, def)
}

export async function updateView(cube: string, name: string, def: ViewDef): Promise<ViewDto> {
  return request<ViewDto>(
    'PUT',
    `/api/v1/cubes/${encodeURIComponent(cube)}/views/${encodeURIComponent(name)}`,
    def,
  )
}

export async function deleteView(cube: string, name: string): Promise<void> {
  return request<void>('DELETE', `/api/v1/cubes/${encodeURIComponent(cube)}/views/${encodeURIComponent(name)}`)
}

export async function executeView(
  cube: string,
  name: string,
  opts?: RequestOptions,
): Promise<CellsetDto> {
  return request<CellsetDto>(
    'POST',
    `/api/v1/cubes/${encodeURIComponent(cube)}/views/${encodeURIComponent(name)}/execute`,
    undefined,
    opts,
  )
}

export async function executeAdhoc(
  cube: string,
  def: ViewDef,
  opts?: RequestOptions,
): Promise<CellsetDto> {
  return request<CellsetDto>('POST', `/api/v1/cubes/${encodeURIComponent(cube)}/cellset`, def, opts)
}

/** Execute a full MDX `SELECT` query (`SELECT <axis> ON COLUMNS, <axis> ON ROWS
 * FROM [cube] [WHERE (...)]`) and return the resulting cellset. A parse or
 * validation failure surfaces as an `ApiError` carrying the server's message. */
export async function executeMdx(
  cube: string,
  mdx: string,
  opts?: RequestOptions,
): Promise<CellsetDto> {
  return request<CellsetDto>('POST', `/api/v1/cubes/${encodeURIComponent(cube)}/mdx`, { mdx }, opts)
}

// ---- rules, explain, feeders, and rule tests (Phase 4) ----

/** A cube's rule source. */
export interface RulesDto {
  source: string
}

/** The structured result of validating a source (rule or flow) without saving it. */
export type SourcePreview =
  | { ok: true }
  | { ok: false; message: string; line?: number; column?: number }

/** The structured result of validating a rule source without saving it. */
export type RulePreview = SourcePreview

/**
 * POST `{ source }` to `path` and convert a parse/compile failure into a
 * structured `{ ok: false }` value (with the message and, when located, the
 * 1-based line/column) instead of throwing, so an editor can mark the error
 * inline. A 401 still clears the token and throws an ApiError(401) (the session
 * expired), uniform with request() so callers can branch on status/instanceof.
 */
async function previewSource(path: string, source: string): Promise<SourcePreview> {
  const response = await fetch(path, {
    method: 'POST',
    headers: authHeaders(true),
    body: JSON.stringify({ source }),
  })
  if (response.ok) return { ok: true }
  if (response.status === 401) throwSessionExpired()
  try {
    const parsed = (await response.json()) as {
      error?: { message?: string; details?: { line?: number; column?: number } }
    }
    return {
      ok: false,
      message: parsed.error?.message ?? `Validation failed (${response.status})`,
      line: parsed.error?.details?.line,
      column: parsed.error?.details?.column,
    }
  } catch {
    return { ok: false, message: `Validation failed (${response.status})` }
  }
}

export async function getRules(cube: string): Promise<RulesDto> {
  return request<RulesDto>('GET', `/api/v1/cubes/${encodeURIComponent(cube)}/rules`)
}

export async function putRules(cube: string, source: string): Promise<RulesDto> {
  return request<RulesDto>('PUT', `/api/v1/cubes/${encodeURIComponent(cube)}/rules`, { source })
}

/**
 * Validate a rule source (parse + compile) without saving. A parse/compile
 * failure resolves to `{ ok: false }` with the message and, when the server
 * located it, the 1-based line/column - so the editor can mark the error
 * inline rather than throwing.
 */
export async function previewRules(cube: string, source: string): Promise<RulePreview> {
  return previewSource(`/api/v1/cubes/${encodeURIComponent(cube)}/rules/preview`, source)
}

export type ExplainDepth = 'full' | 'immediate'

/** One node of a provenance ("explain") trace. */
export interface TraceDto {
  cube: string
  coord: string[]
  value: string
  kind: 'stored' | 'rule' | 'consolidation'
  rule?: number
  span_start?: number
  span_end?: number
  contributions?: number
  inputs: TraceDto[]
}

export async function explainCell(
  cube: string,
  coord: Coord,
  depth: ExplainDepth = 'full',
): Promise<TraceDto> {
  return request<TraceDto>('POST', `/api/v1/cubes/${encodeURIComponent(cube)}/cells/explain`, {
    coord,
    depth,
  })
}

/** A rule whose feeders could not be auto-inferred, with the reason. */
export interface OpaqueRuleDto {
  rule: number
  reason: string
}

/** Auto-inferred feeders plus under/over-feed validation for a cube. */
export interface FeederReportDto {
  fed_cell_count: number
  under_fed: string[][]
  over_fed: string[][]
  estimated_over_fed_bytes: number
  opaque_rules: OpaqueRuleDto[]
}

export async function feederDiagnostics(cube: string): Promise<FeederReportDto> {
  return request<FeederReportDto>(
    'GET',
    `/api/v1/cubes/${encodeURIComponent(cube)}/feeders/diagnostics`,
  )
}

/** A fixture or assertion cell in a rule test. */
export interface TestCellDto {
  coord: Coord
  value: string
}

/** A rule unit test: fixtures set leaves, assertions check derived values. */
export interface RuleTestDto {
  name: string
  fixtures: TestCellDto[]
  assertions: TestCellDto[]
}

export async function listRuleTests(cube: string): Promise<RuleTestDto[]> {
  const result = await request<{ tests: RuleTestDto[] }>(
    'GET',
    `/api/v1/cubes/${encodeURIComponent(cube)}/rules/tests`,
  )
  return result.tests
}

export async function putRuleTest(cube: string, test: RuleTestDto): Promise<RuleTestDto> {
  return request<RuleTestDto>(
    'POST',
    `/api/v1/cubes/${encodeURIComponent(cube)}/rules/tests`,
    test,
  )
}

export async function deleteRuleTest(cube: string, name: string): Promise<void> {
  return request<void>(
    'DELETE',
    `/api/v1/cubes/${encodeURIComponent(cube)}/rules/tests/${encodeURIComponent(name)}`,
  )
}

/** One failed assertion within a rule test run. */
export interface AssertionFailureDto {
  coord: Coord
  expected: string
  actual: string
}

export interface TestOutcomeDto {
  name: string
  passed: boolean
  failures: AssertionFailureDto[]
}

export interface TestReportDto {
  all_passed: boolean
  outcomes: TestOutcomeDto[]
}

export async function runRuleTests(cube: string): Promise<TestReportDto> {
  return request<TestReportDto>(
    'POST',
    `/api/v1/cubes/${encodeURIComponent(cube)}/rules/tests/run`,
  )
}

// ---- flows (Phase 5; server-global, ADR-0035) ----

/** One declared data source on a flow (ADR-0035). A `global` input references a
 * server-global connection by its name (the connection itself lives in the
 * global store, so `connection` is omitted); a `local` (flow-scoped) input
 * carries its own embedded ConnectionDto whose name mirrors `name`. In code a
 * global source reads as `ctx.input('NAME')` and a local source as
 * `ctx.input('local.NAME')`. */
export interface FlowInputDto {
  name: string
  scope: 'global' | 'local'
  connection?: ConnectionDto | null
}

/** A flow: name, TypeScript source, recorded owner, an optional default target
 * cube (a back-compatibility convenience, not ownership), and its declared data
 * sources (ADR-0035). Flows are server-global, not owned by any cube. */
export interface FlowDto {
  name: string
  source: string
  /** The principal a scheduled run executes as (read-only in the UI). */
  owner?: string | null
  /** A convenience target for legacy cube-less calls (ctx.writeCells, ...). */
  default_cube?: string | null
  /** The flow's declared data sources (global references + flow-scoped). */
  inputs: FlowInputDto[]
}

/** The structured result of validating a flow source without saving it. */
export type FlowPreview = SourcePreview

/** A flow run report. */
export interface RunReport {
  rows_read: number
  cells_written: number
  elements_added: number
  logs: string[]
}

const FLOW_BASE = '/api/v1/flows'

export async function listFlows(): Promise<FlowDto[]> {
  const result = await request<{ flows: FlowDto[] }>('GET', FLOW_BASE)
  return result.flows
}

export async function getFlow(name: string): Promise<FlowDto> {
  return request<FlowDto>('GET', `${FLOW_BASE}/${encodeURIComponent(name)}`)
}

export async function putFlow(flow: FlowDto): Promise<FlowDto> {
  return request<FlowDto>('PUT', `${FLOW_BASE}/${encodeURIComponent(flow.name)}`, flow)
}

export async function deleteFlow(name: string): Promise<void> {
  return request<void>('DELETE', `${FLOW_BASE}/${encodeURIComponent(name)}`)
}

/**
 * Validate a flow source (strip + parse) without saving. A failure resolves to
 * `{ ok: false }` with the message and, when located, the line/column - so the
 * editor can mark the error inline rather than throwing.
 */
export async function previewFlow(source: string): Promise<FlowPreview> {
  return previewSource(`${FLOW_BASE}/preview`, source)
}

/** Run a flow. With declared inputs the backend fetches every source, so a plain
 * run only needs `params`. For quick testing an ad-hoc inline payload may pass a
 * single CSV body (`input`) or a name->content map (`inputs`) for a named source,
 * or a named `connection` to fetch. */
export async function runFlow(
  name: string,
  body: {
    input?: string
    connection?: string
    inputs?: Record<string, string>
    params?: Record<string, string>
  },
): Promise<RunReport> {
  return request<RunReport>('POST', `${FLOW_BASE}/${encodeURIComponent(name)}/run`, body)
}

// ---- connections (ADR-0012) ----

/** A static HTTP request header (ADR-0030). */
export interface ConnectionHeaderDto {
  name: string
  value: string
}

/** An HTTP credential reference: scheme + the NAME of a stored secret (ADR-0030). */
export interface ConnectionAuthDto {
  kind: string
  secret: string
}

/** A data-source connection: `command` (ADR-0012) or `http` (ADR-0030). */
export interface ConnectionDto {
  name: string
  kind: string
  program: string
  args: string[]
  format: string
  json_path?: string | null
  timeout_ms: number
  /** Optional absolute working directory the program runs in (ADR-0012 addendum). */
  working_dir?: string | null
  /** HTTP url (kind === 'http'). */
  url?: string
  /** Static HTTP request headers (kind === 'http'). */
  headers?: ConnectionHeaderDto[]
  /** HTTP credential, referencing a secret by name (kind === 'http'). */
  auth?: ConnectionAuthDto | null
  // ---- sql fields (kind === 'sql', ADR-0034) ----
  /** Database engine: 'postgres' or 'mysql' (MySQL/MariaDB). */
  engine?: string
  /** Database host. */
  host?: string
  /** Database port. */
  port?: number
  /** Database (catalog) name. */
  database?: string
  /** Connecting user. */
  user?: string
  /** Name of the secret holding the password (never the value). */
  password_secret?: string | null
  /** The SQL query to run. */
  query?: string
  /** TLS mode: 'verify-full' (default), 'require', or 'disable'. */
  ssl_mode?: string
}

/** A connection's sample output, from the wizard's "Test" button (ADR-0027). */
export interface ConnectionPreview {
  columns: string[]
  rows: string[][]
  row_count: number
}

const CONN_BASE = '/api/v1/connections'

export async function listConnections(): Promise<ConnectionDto[]> {
  const result = await request<{ connections: ConnectionDto[] }>('GET', CONN_BASE)
  return result.connections
}

export async function getConnection(name: string): Promise<ConnectionDto> {
  return request<ConnectionDto>('GET', `${CONN_BASE}/${encodeURIComponent(name)}`)
}

export async function putConnection(conn: ConnectionDto): Promise<ConnectionDto> {
  return request<ConnectionDto>('PUT', `${CONN_BASE}/${encodeURIComponent(conn.name)}`, conn)
}

export async function deleteConnection(name: string): Promise<void> {
  return request<void>('DELETE', `${CONN_BASE}/${encodeURIComponent(name)}`)
}

/** Run a connection and return up to 20 sample rows plus the total count. */
export async function previewConnection(name: string): Promise<ConnectionPreview> {
  return request<ConnectionPreview>('POST', `${CONN_BASE}/${encodeURIComponent(name)}/preview`)
}

// ---- operator secrets (ADR-0030, admin) ----

/** The secret names (admin). Values are write-only and never returned. */
export async function listSecrets(): Promise<string[]> {
  const result = await request<{ names: string[] }>('GET', '/api/v1/secrets')
  return result.names
}

/** Set (create or replace) a secret value (admin). The value is never echoed. */
export async function putSecret(name: string, value: string): Promise<void> {
  return request<void>('PUT', `/api/v1/secrets/${encodeURIComponent(name)}`, { value })
}

/** Delete a secret (admin). */
export async function deleteSecret(name: string): Promise<void> {
  return request<void>('DELETE', `/api/v1/secrets/${encodeURIComponent(name)}`)
}

export interface ImportRequest {
  csv: string
  columns: Record<string, string>
  value_column: string
  fixed?: Record<string, string>
}

/** CSV import stays cube-scoped (ADR-0035 keeps `POST /cubes/{cube}/import`). */
export async function importCsv(cube: string, req: ImportRequest): Promise<RunReport> {
  return request<RunReport>('POST', `/api/v1/cubes/${encodeURIComponent(cube)}/import`, req)
}

/** A flow unit test (server-global, ADR-0035). `input` is the single-source
 * inline content; `inputs` maps a source address to inline content for the
 * multi-source case; `cube` is the target cube the assertions check (blank =
 * none in particular). */
export interface FlowTestDto {
  name: string
  flow: string
  input: string
  params: Record<string, string>
  assertions: TestCellDto[]
  /** Source address -> inline content (multi-source tests, ADR-0035). */
  inputs?: Record<string, string>
  /** The target cube the assertions check; blank/null targets none specifically. */
  cube?: string | null
}

const FLOW_TESTS_BASE = `${FLOW_BASE}/tests`

export async function listFlowTests(): Promise<FlowTestDto[]> {
  const result = await request<{ tests: FlowTestDto[] }>('GET', FLOW_TESTS_BASE)
  return result.tests
}

export async function putFlowTest(test: FlowTestDto): Promise<FlowTestDto> {
  return request<FlowTestDto>('POST', FLOW_TESTS_BASE, test)
}

export async function deleteFlowTest(name: string): Promise<void> {
  return request<void>('DELETE', `${FLOW_TESTS_BASE}/${encodeURIComponent(name)}`)
}

export async function runFlowTests(): Promise<TestReportDto> {
  return request<TestReportDto>('POST', `${FLOW_TESTS_BASE}/run`)
}

// ---- schedules + run history (Phase 8, ADR-0013; server-global, ADR-0035) ----

/** A schedule: an ordered list of flow steps run on a fixed interval. Schedules
 * are server-global (ADR-0035); the cubes written are whatever its flows name. */
export interface ScheduleDto {
  name: string
  /** Flow names, run in order each firing. */
  steps: string[]
  /** Interval between firings, in milliseconds. */
  every_millis: number
  enabled: boolean
}

/** One execution of a schedule or flow, as recorded in the durable run ledger.
 * `cube` may be empty for a global run (ADR-0035): show a dash, do not assume a
 * cube. */
export interface RunDto {
  id: string
  cube: string
  /** The schedule name (when is_job) or flow name. */
  target: string
  is_job: boolean
  fire_millis: number
  state: 'pending' | 'running' | 'succeeded' | 'failed' | string
  rows_read: number
  cells_written: number
  elements_added: number
  error?: string
  principal: string
}

const SCHED_BASE = '/api/v1/schedules'

export async function listSchedules(): Promise<ScheduleDto[]> {
  const result = await request<{ schedules: ScheduleDto[] }>('GET', SCHED_BASE)
  return result.schedules
}

export async function getSchedule(name: string): Promise<ScheduleDto> {
  return request<ScheduleDto>('GET', `${SCHED_BASE}/${encodeURIComponent(name)}`)
}

/** Create or replace a schedule. Each step must name an existing flow. */
export async function putSchedule(schedule: ScheduleDto): Promise<ScheduleDto> {
  return request<ScheduleDto>('PUT', `${SCHED_BASE}/${encodeURIComponent(schedule.name)}`, schedule)
}

export async function deleteSchedule(name: string): Promise<void> {
  await request<void>('DELETE', `${SCHED_BASE}/${encodeURIComponent(name)}`)
}

/** Run a schedule now (manual kick); resolves with the resulting run record. */
export async function runSchedule(name: string): Promise<RunDto> {
  return request<RunDto>('POST', `${SCHED_BASE}/${encodeURIComponent(name)}/run`)
}

/** Recent runs across the server, newest first (admin only). */
export async function listRuns(limit = 50): Promise<RunDto[]> {
  const result = await request<{ runs: RunDto[] }>('GET', `/api/v1/runs?limit=${limit}`)
  return result.runs
}

/** View-cache counters for the admin server overview (ADR-0028). */
export interface ViewCacheStats {
  enabled: boolean
  hits: number
  misses: number
  entries: number
}

/** Server-wide stats for the admin overview (admin only). */
export async function getOverview(): Promise<{ view_cache: ViewCacheStats }> {
  return request<{ view_cache: ViewCacheStats }>('GET', '/api/v1/overview')
}

// ---- model editing (ADR-0021) ----

/** An element to add: a name and its kind. */
export interface NewElement {
  name: string
  kind: ElementKind
}

/** A consolidation edge: a consolidated parent rolls up a child with a weight. */
export interface NewEdge {
  parent: string
  child: string
  weight?: number
}

/** A dimension to declare when creating a cube: either an inline definition
 * (name + members + edges) or a reference to a registered shared dimension
 * (`ref` = its id; name/elements/edges are then ignored). (ADR-0021, ADR-0024) */
export interface NewDimension {
  name: string
  elements?: NewElement[]
  edges?: NewEdge[]
  /** Reference a shared dimension by id; the cube materializes a copy of it. */
  ref?: number
}

/** The result of a model-editing commit. */
export interface CommitResult {
  version: number
  /** Newly-created element count (element adds only). */
  elements_added?: number
}

/** Create a new cube with its dimensions and initial members (admin only). */
export async function createCube(
  name: string,
  dimensions: NewDimension[],
): Promise<CommitResult> {
  return request<CommitResult>('POST', '/api/v1/cubes', { name, dimensions })
}

/** Add elements and consolidation edges to existing dimensions (append-only). */
export async function addElements(
  cube: string,
  elements: { dimension: string; name: string; kind: ElementKind }[],
  edges: { dimension: string; parent: string; child: string; weight?: number }[] = [],
): Promise<CommitResult> {
  return request<CommitResult>('POST', `/api/v1/cubes/${encodeURIComponent(cube)}/elements`, {
    elements,
    edges,
  })
}

export type AttributeKind = 'text' | 'numeric' | 'alias'

/** Define an attribute on a dimension. */
export async function defineAttribute(
  cube: string,
  dimension: string,
  attribute: string,
  kind: AttributeKind,
): Promise<CommitResult> {
  return request<CommitResult>(
    'PUT',
    `/api/v1/cubes/${encodeURIComponent(cube)}/dimensions/${encodeURIComponent(
      dimension,
    )}/attributes/${encodeURIComponent(attribute)}`,
    { kind },
  )
}

/** Set an attribute's value for one or more elements. */
export async function setAttributeValues(
  cube: string,
  dimension: string,
  attribute: string,
  values: { element: string; value: string }[],
): Promise<CommitResult> {
  return request<CommitResult>(
    'PUT',
    `/api/v1/cubes/${encodeURIComponent(cube)}/dimensions/${encodeURIComponent(
      dimension,
    )}/attributes/${encodeURIComponent(attribute)}/values`,
    { values },
  )
}

// ---- shared dimension library (ADR-0024) ----

/** A shared dimension as summarized in the library listing. */
export interface SharedDimensionSummary {
  id: number
  name: string
  generation: number
  element_count: number
  references: string[]
}

/** A shared dimension's full definition. */
export interface SharedDimensionDetail {
  id: number
  name: string
  generation: number
  references: string[]
  elements: ElementDto[]
  edges: EdgeDto[]
  /** Attribute columns (defs + per-element values), carried so the dimension
   * editor shows them; element-masked per the caller (ADR-0024/0033). */
  attributes?: AttributeDto[]
}

export async function listDimensions(): Promise<SharedDimensionSummary[]> {
  return request<SharedDimensionSummary[]>('GET', '/api/v1/dimensions')
}

export async function getDimension(id: number): Promise<SharedDimensionDetail> {
  return request<SharedDimensionDetail>('GET', `/api/v1/dimensions/${id}`)
}

/** Register a reusable shared dimension; resolves with its new id and generation. */
export async function registerDimension(def: {
  name: string
  elements?: NewElement[]
  edges?: NewEdge[]
}): Promise<{ id: number; name: string; generation: number }> {
  return request('POST', '/api/v1/dimensions', def)
}

/** Append members/edges to a shared dimension; fans out to referencing cubes. */
export async function growDimension(
  id: number,
  elements: NewElement[],
  edges: NewEdge[] = [],
): Promise<{ id: number; generation: number; fanned_out_to: string[] }> {
  return request('POST', `/api/v1/dimensions/${id}/elements`, { elements, edges })
}

/** Delete an unreferenced shared dimension (409 if any cube still references it). */
export async function deleteDimension(id: number): Promise<void> {
  return request<void>('DELETE', `/api/v1/dimensions/${id}`)
}

// ---- structural dimension editing (ADR-0036) ----

/** Where to place a newly inserted member, relative to an existing one (or at
 * the end). `ref` is required for `before`/`after`. */
export interface InsertPosition {
  at: 'end' | 'before' | 'after'
  ref?: string
}

/** A single structural edit on a dimension, tagged by `op` (ADR-0036). Each
 * changes the dimension transactionally and is reflected in a new committed
 * version. `reorder` carries a full permutation of the current member names;
 * `reparent` with `new_parent: null` detaches a member to a root. `add_child`
 * adds a member to a consolidation additively, keeping its existing parents (a
 * member may roll up to multiple consolidations); unlike `reparent` it never
 * detaches the child from any other consolidation. `remove_child` removes just
 * the one `parent` -> `child` edge, keeping the member, its data, and its other
 * parents (idempotent when the edge is absent); unlike `reparent: null` it does
 * not detach every parent, and unlike `delete` it keeps the member. */
export type DimensionEdit =
  | { op: 'reorder'; new_order: string[] }
  | { op: 'reparent'; child: string; new_parent: string | null; weight?: number }
  | { op: 'add_child'; parent: string; child: string; weight?: number }
  | { op: 'remove_child'; parent: string; child: string }
  | { op: 'set_kind'; element: string; kind: ElementKind }
  | { op: 'delete'; element: string }
  | { op: 'insert'; name: string; kind: ElementKind; position: InsertPosition }
  // Explicit top-level membership (ADR-0038): pin a member so it is shown as a
  // display root in addition to wherever it rolls up; unpin removes that pin.
  // Neither changes any rollup edge or stored value.
  | { op: 'pin_to_top'; element: string }
  | { op: 'unpin_from_top'; element: string }

/** Apply one structural edit to a cube-embedded dimension (ADR-0036). Resolves
 * with the new committed version. A 422 carries the rejection reason (e.g. a
 * non-permutation reorder or a cycle); a 403 means missing Dimension:Write or an
 * element-security denial. */
export async function editCubeDimension(
  cube: string,
  dim: string,
  edit: DimensionEdit,
): Promise<{ version: number }> {
  return request<{ version: number }>(
    'POST',
    `/api/v1/cubes/${encodeURIComponent(cube)}/dimensions/${encodeURIComponent(dim)}/edit`,
    edit,
  )
}

/** Apply one structural edit to a registry (global) dimension (ADR-0036). The
 * same remap fans out to every referencing cube, so the result names which
 * cubes were updated (`fanned_out_to`). */
export async function editDimensionById(
  id: number,
  edit: DimensionEdit,
): Promise<{ version: number; fanned_out_to: string[] }> {
  return request<{ version: number; fanned_out_to: string[] }>(
    'POST',
    `/api/v1/dimensions/${id}/edit`,
    edit,
  )
}

/** Promote a cube's embedded dimension into the global registry (ADR-0031) so
 * other cubes can reference it; resolves with the new global dimension id. The
 * cube keeps its data unchanged. 409 if the dimension is already global. */
export async function promoteDimension(
  cube: string,
  dim: string,
): Promise<{ id: number; name: string; generation: number }> {
  return request(
    'POST',
    `/api/v1/cubes/${encodeURIComponent(cube)}/dimensions/${encodeURIComponent(dim)}/promote`,
  )
}

// ---- security administration (Phase 7, ADR-0015 + ADR-0010, admin only) ----

export type AccessLevel = 'none' | 'read' | 'write' | 'admin'
export type SubjectKind = 'user' | 'group'

/** A user as returned by the admin listing. */
export interface UserDto {
  username: string
  is_admin: boolean
  groups: string[]
}

/** An element access grant (level `none` revokes). */
export interface ElementGrantDto {
  cube: string
  dimension: string
  element: string
  subject_kind: SubjectKind
  subject: string
  level: AccessLevel
}

/** One audit record (ADR-0010). */
export interface AuditRecordDto {
  seq: number
  timestamp_millis: number
  actor: string
  action: string
  object_kind: string
  target: string
  allowed: boolean
}

export async function listUsers(): Promise<UserDto[]> {
  const result = await request<{ users: UserDto[] }>('GET', '/api/v1/users')
  return result.users
}

export async function createUser(body: {
  username: string
  password: string
  is_admin?: boolean
  groups?: string[]
}): Promise<void> {
  return request<void>('POST', '/api/v1/users', body)
}

export async function patchUser(
  username: string,
  body: { is_admin?: boolean; groups?: string[]; password?: string },
): Promise<void> {
  return request<void>('PATCH', `/api/v1/users/${encodeURIComponent(username)}`, body)
}

export async function deleteUser(username: string): Promise<void> {
  return request<void>('DELETE', `/api/v1/users/${encodeURIComponent(username)}`)
}

/**
 * Reset a user to a system-generated temporary password and require a change at
 * next sign-in (admin). The temporary password is returned once; it is never
 * retrievable again.
 */
export async function resetUserPassword(
  username: string,
): Promise<{ username: string; temp_password: string }> {
  return request('POST', `/api/v1/users/${encodeURIComponent(username)}/reset-password`)
}

export async function listGroups(): Promise<string[]> {
  const result = await request<{ groups: string[] }>('GET', '/api/v1/groups')
  return result.groups
}

export async function createGroup(name: string): Promise<void> {
  return request<void>('POST', '/api/v1/groups', { name })
}

export async function deleteGroup(name: string): Promise<void> {
  return request<void>('DELETE', `/api/v1/groups/${encodeURIComponent(name)}`)
}

export async function listElementAcls(): Promise<ElementGrantDto[]> {
  const result = await request<{ grants: ElementGrantDto[] }>('GET', '/api/v1/acl/elements')
  return result.grants
}

export async function putElementAcl(grant: ElementGrantDto): Promise<void> {
  return request<void>('PUT', '/api/v1/acl/elements', grant)
}

// ---- modular per-object-kind grants / roles (ADR-0023) ----

/** A securable object kind that can be granted per-scope. */
export type GrantKind =
  | 'cube'
  | 'dimension'
  | 'rule'
  | 'flow'
  | 'view'
  | 'subset'
  | 'job'
  | 'connection'
  | 'sandbox'

/** One modular grant: a subject's level on a kind within a scope. */
export interface GrantDto {
  subject_kind: SubjectKind
  subject: string
  scope: 'global' | 'cube'
  cube?: string
  kind: GrantKind
  level: AccessLevel
}

export async function listGrants(): Promise<GrantDto[]> {
  const result = await request<{ grants: GrantDto[] }>('GET', '/api/v1/acl/grants')
  return result.grants
}

/** Set (or, with level `none`, revoke) a per-kind grant for a user or group. */
export async function setGrant(grant: GrantDto): Promise<void> {
  return request<void>('PUT', '/api/v1/acl/grants', grant)
}

/** The shell a user sees (ADR-0020 progressive disclosure). Ordered by breadth:
 * `business` (Views / data-entry only) < `modeler` (Dimensions, Rules, Flows,
 * the full model tree, and raw-MDX affordances) < `admin` (modeler shell plus the
 * Administration surface). Derived from the grant lattice, not `is_admin` alone,
 * so a non-admin modeler is never denied MDX/rules/flows they legitimately hold a
 * grant for. */
export type Persona = 'business' | 'modeler' | 'admin'

/** The object kinds whose WRITE grant marks a user as a modeler (they author the
 * model itself), per ADR-0020: any dimension/rule/flow write grant ⇒ modeler. */
const MODELER_KINDS: ReadonlySet<GrantKind> = new Set<GrantKind>(['dimension', 'rule', 'flow'])

/**
 * Resolve a user's persona from the modular grant lattice (ADR-0020). A caller
 * holding admin on ANY object kind - or flagged `is_admin` - is an admin; a
 * caller with a write (or admin) grant on any dimension, rule, or flow is a
 * modeler; otherwise a business user. `grants` is the set of grants that apply to
 * THIS caller (already narrowed to the user and their groups); passing the raw
 * global list would over-promote everyone, so callers must pre-filter.
 *
 * NOTE (deliberate fail-open): `GET /api/v1/acl/grants` is admin-only server-side
 * (`require_admin`), and `auth/me` exposes no per-caller capability set, so a
 * non-admin's own grants are NOT enumerable from the browser. Callers therefore
 * pass `[]` for a non-admin and rely on `fallback` (see CubeApp), which defaults a
 * non-admin to `modeler` rather than `business`: ADR-0020 is explicit that wrongly
 * hiding MDX/rules/flows from a non-admin modeler is the failure to avoid. A true
 * business shell for a non-admin needs a server-provided effective-capability
 * signal (a follow-up), at which point this function computes it directly. */
export function personaFromGrants(grants: readonly GrantDto[], isAdmin: boolean): Persona {
  if (isAdmin) return 'admin'
  if (grants.some((g) => g.level === 'admin')) return 'admin'
  if (grants.some((g) => MODELER_KINDS.has(g.kind) && (g.level === 'write' || g.level === 'admin'))) {
    return 'modeler'
  }
  return 'business'
}

/** Audit-query filters; omitted fields are not constrained. */
export interface AuditQuery {
  actor?: string
  action?: string
  outcome?: 'allowed' | 'denied'
  limit?: number
}

export async function queryAudit(filter: AuditQuery): Promise<AuditRecordDto[]> {
  const params = new URLSearchParams()
  if (filter.actor) params.set('actor', filter.actor)
  if (filter.action) params.set('action', filter.action)
  if (filter.outcome) params.set('outcome', filter.outcome)
  if (filter.limit) params.set('limit', String(filter.limit))
  const query = params.toString()
  const result = await request<{ records: AuditRecordDto[] }>(
    'GET',
    `/api/v1/audit${query ? `?${query}` : ''}`,
  )
  return result.records
}

export interface ChangeEvent {
  type: string
  cube?: string
  version?: number
  coords?: Coord[]
}

/** Open the change-event WebSocket (authenticated by the session cookie). */
export function connectWs(onEvent: (event: ChangeEvent) => void): WebSocket {
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws'
  const socket = new WebSocket(`${scheme}://${location.host}/api/v1/ws`)
  socket.onmessage = (message) => {
    try {
      onEvent(JSON.parse(message.data as string) as ChangeEvent)
    } catch {
      /* ignore malformed frames */
    }
  }
  return socket
}
