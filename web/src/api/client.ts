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
