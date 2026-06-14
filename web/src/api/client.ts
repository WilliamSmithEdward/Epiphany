// Typed client for the Epiphany REST API. Numeric cell values are decimal
// STRINGS, never JS numbers (ADR-0008), so they never lose precision. The
// session token is kept in memory (not localStorage); the server also sets an
// HttpOnly cookie, which authenticates the WebSocket.

export type Coord = Record<string, string>
export type ElementKind = 'numeric' | 'string' | 'consolidated'

export interface ElementDto {
  name: string
  kind: ElementKind
}

export interface EdgeDto {
  parent: string
  child: string
  weight: number
}

export interface DimensionDto {
  name: string
  elements: ElementDto[]
  edges: EdgeDto[]
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
}

export interface LoginResult {
  token: string
  user: { username: string; is_admin: boolean; must_change_password: boolean }
}

export interface BatchResult {
  applied: number
  version: number
}

let token: string | null = null

export function setToken(value: string | null): void {
  token = value
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {}
  if (token) headers['authorization'] = `Bearer ${token}`
  if (body !== undefined) headers['content-type'] = 'application/json'
  const response = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  if (response.status === 401) {
    setToken(null)
    throw new Error('Your session has expired. Please sign in again.')
  }
  if (!response.ok) {
    let message = `Request failed (${response.status})`
    try {
      const parsed = (await response.json()) as { error?: { message?: string } }
      if (parsed.error?.message) message = parsed.error.message
    } catch {
      /* keep the default message */
    }
    throw new Error(message)
  }
  if (response.status === 204) return undefined as T
  return (await response.json()) as T
}

export async function login(username: string, password: string): Promise<LoginResult> {
  const result = await request<LoginResult>('POST', '/api/v1/auth/login', { username, password })
  setToken(result.token)
  return result
}

export async function logout(): Promise<void> {
  try {
    await request<void>('POST', '/api/v1/auth/logout')
  } finally {
    setToken(null)
  }
}

export async function listCubes(): Promise<CubeSummary[]> {
  const result = await request<{ cubes: CubeSummary[] }>('GET', '/api/v1/cubes')
  return result.cubes
}

export async function getCube(cube: string): Promise<CubeDetail> {
  return request<CubeDetail>('GET', `/api/v1/cubes/${encodeURIComponent(cube)}`)
}

export async function readCells(cube: string, coords: Coord[]): Promise<CellDto[]> {
  const result = await request<{ cells: CellDto[] }>(
    'POST',
    `/api/v1/cubes/${encodeURIComponent(cube)}/cells/read`,
    { coords },
  )
  return result.cells
}

export async function writeCell(cube: string, coord: Coord, value: string): Promise<CellDto> {
  return request<CellDto>('PUT', `/api/v1/cubes/${encodeURIComponent(cube)}/cell`, { coord, value })
}

export async function batchWrite(
  cube: string,
  writes: { coord: Coord; value: string }[],
): Promise<BatchResult> {
  return request<BatchResult>('POST', `/api/v1/cubes/${encodeURIComponent(cube)}/cells/batch`, {
    writes,
  })
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
  suppress_zeros?: boolean
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
  suppress_zeros: boolean
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

export async function getSubset(cube: string, dim: string, name: string): Promise<SubsetDto> {
  return request<SubsetDto>('GET', `${dimBase(cube, dim)}/subsets/${encodeURIComponent(name)}`)
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

export async function subsetMembers(cube: string, dim: string, name: string): Promise<MemberDto[]> {
  const result = await request<{ members: MemberDto[] }>(
    'GET',
    `${dimBase(cube, dim)}/subsets/${encodeURIComponent(name)}/members`,
  )
  return result.members
}

export async function previewSubset(cube: string, dim: string, def: SubsetDef): Promise<MemberDto[]> {
  const result = await request<{ members: MemberDto[] }>(
    'POST',
    `${dimBase(cube, dim)}/subsets/preview`,
    def,
  )
  return result.members
}

export async function previewMdx(cube: string, dim: string, mdx: string): Promise<MemberDto[]> {
  const result = await request<{ members: MemberDto[] }>('POST', `${dimBase(cube, dim)}/mdx/preview`, {
    mdx,
  })
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

export async function executeView(cube: string, name: string): Promise<CellsetDto> {
  return request<CellsetDto>(
    'POST',
    `/api/v1/cubes/${encodeURIComponent(cube)}/views/${encodeURIComponent(name)}/execute`,
  )
}

export async function executeAdhoc(cube: string, def: ViewDef): Promise<CellsetDto> {
  return request<CellsetDto>('POST', `/api/v1/cubes/${encodeURIComponent(cube)}/cellset`, def)
}

// ---- rules, explain, feeders, and rule tests (Phase 4) ----

/** A cube's rule source. */
export interface RulesDto {
  source: string
}

/** The structured result of validating a rule source without saving it. */
export type RulePreview =
  | { ok: true }
  | { ok: false; message: string; line?: number; column?: number }

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
  const headers: Record<string, string> = { 'content-type': 'application/json' }
  if (token) headers['authorization'] = `Bearer ${token}`
  const response = await fetch(`/api/v1/cubes/${encodeURIComponent(cube)}/rules/preview`, {
    method: 'POST',
    headers,
    body: JSON.stringify({ source }),
  })
  if (response.ok) return { ok: true }
  if (response.status === 401) {
    setToken(null)
    throw new Error('Your session has expired. Please sign in again.')
  }
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

export type ExplainDepth = 'full' | 'immediate' | string

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

// ---- flows (Phase 5) ----

/** A flow: name and TypeScript source. */
export interface FlowDto {
  name: string
  source: string
}

/** The structured result of validating a flow source without saving it. */
export type FlowPreview =
  | { ok: true }
  | { ok: false; message: string; line?: number; column?: number }

/** A flow run report. */
export interface RunReport {
  rows_read: number
  cells_written: number
  elements_added: number
  logs: string[]
}

function flowBase(cube: string): string {
  return `/api/v1/cubes/${encodeURIComponent(cube)}/flows`
}

export async function listFlows(cube: string): Promise<FlowDto[]> {
  const result = await request<{ flows: FlowDto[] }>('GET', flowBase(cube))
  return result.flows
}

export async function putFlow(cube: string, name: string, source: string): Promise<FlowDto> {
  return request<FlowDto>('PUT', `${flowBase(cube)}/${encodeURIComponent(name)}`, { name, source })
}

export async function deleteFlow(cube: string, name: string): Promise<void> {
  return request<void>('DELETE', `${flowBase(cube)}/${encodeURIComponent(name)}`)
}

/**
 * Validate a flow source (strip + parse) without saving. A failure resolves to
 * `{ ok: false }` with the message and, when located, the line/column - so the
 * editor can mark the error inline rather than throwing.
 */
export async function previewFlow(cube: string, source: string): Promise<FlowPreview> {
  const headers: Record<string, string> = { 'content-type': 'application/json' }
  if (token) headers['authorization'] = `Bearer ${token}`
  const response = await fetch(`${flowBase(cube)}/preview`, {
    method: 'POST',
    headers,
    body: JSON.stringify({ source }),
  })
  if (response.ok) return { ok: true }
  if (response.status === 401) {
    setToken(null)
    throw new Error('Your session has expired. Please sign in again.')
  }
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

export async function runFlow(
  cube: string,
  name: string,
  input: string,
  params: Record<string, string> = {},
): Promise<RunReport> {
  return request<RunReport>('POST', `${flowBase(cube)}/${encodeURIComponent(name)}/run`, {
    input,
    params,
  })
}

export interface ImportRequest {
  csv: string
  columns: Record<string, string>
  value_column: string
  fixed?: Record<string, string>
}

export async function importCsv(cube: string, req: ImportRequest): Promise<RunReport> {
  return request<RunReport>('POST', `${flowBase(cube)}/import`, req)
}

/** A flow unit test. */
export interface FlowTestDto {
  name: string
  flow: string
  input: string
  params: Record<string, string>
  assertions: TestCellDto[]
}

export async function listFlowTests(cube: string): Promise<FlowTestDto[]> {
  const result = await request<{ tests: FlowTestDto[] }>('GET', `${flowBase(cube)}/tests`)
  return result.tests
}

export async function putFlowTest(cube: string, test: FlowTestDto): Promise<FlowTestDto> {
  return request<FlowTestDto>('POST', `${flowBase(cube)}/tests`, test)
}

export async function deleteFlowTest(cube: string, name: string): Promise<void> {
  return request<void>('DELETE', `${flowBase(cube)}/tests/${encodeURIComponent(name)}`)
}

export async function runFlowTests(cube: string): Promise<TestReportDto> {
  return request<TestReportDto>('POST', `${flowBase(cube)}/tests/run`)
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
