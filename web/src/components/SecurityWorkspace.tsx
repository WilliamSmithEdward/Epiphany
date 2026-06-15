import { useCallback, useEffect, useState } from 'react'
import {
  createGroup,
  createUser,
  deleteGroup,
  deleteUser,
  listCubeGrants,
  listElementAcls,
  listGroups,
  listObjectAcls,
  listUsers,
  patchUser,
  putCubeGrant,
  putElementAcl,
  putObjectAcl,
  type AccessLevel,
  type CubeGrantDto,
  type CubeGrantLevel,
  type ElementGrantDto,
  type ObjectGrantDto,
  type SubjectKind,
  type UserDto,
} from '../api/client'
import AuditViewer from './AuditViewer'

type Tab = 'users' | 'groups' | 'cube-grants' | 'objects' | 'elements' | 'audit'

const OBJECT_KINDS = [
  'cube',
  'dimension',
  'rule',
  'flow',
  'view',
  'subset',
  'connection',
  'sandbox',
]
const LEVELS: AccessLevel[] = ['read', 'write', 'admin']
const CUBE_GRANT_LEVELS: CubeGrantLevel[] = ['read', 'write', 'admin', 'deny']

// The server-global security console (ADR-0015 + ADR-0010): users, groups,
// object and element access, and the audit log. Admin only; the topbar hides the
// entry point for everyone else and every route is server-gated regardless.
export default function SecurityWorkspace() {
  const [tab, setTab] = useState<Tab>('users')
  return (
    <div>
      <div className="tabs">
        <button className={tab === 'users' ? 'active' : ''} onClick={() => setTab('users')}>
          Users
        </button>
        <button className={tab === 'groups' ? 'active' : ''} onClick={() => setTab('groups')}>
          Groups
        </button>
        <button
          className={tab === 'cube-grants' ? 'active' : ''}
          onClick={() => setTab('cube-grants')}
        >
          Cube grants
        </button>
        <button className={tab === 'objects' ? 'active' : ''} onClick={() => setTab('objects')}>
          Object access
        </button>
        <button className={tab === 'elements' ? 'active' : ''} onClick={() => setTab('elements')}>
          Element access
        </button>
        <button className={tab === 'audit' ? 'active' : ''} onClick={() => setTab('audit')}>
          Audit
        </button>
      </div>
      {tab === 'users' ? <UsersTab /> : null}
      {tab === 'groups' ? <GroupsTab /> : null}
      {tab === 'cube-grants' ? <CubeGrantsTab /> : null}
      {tab === 'objects' ? <ObjectAclTab /> : null}
      {tab === 'elements' ? <ElementAclTab /> : null}
      {tab === 'audit' ? <AuditViewer /> : null}
    </div>
  )
}

/** Run an admin action, surfacing any error and reloading on success. */
function useAction(reload: () => void, setError: (m: string | null) => void) {
  return useCallback(
    (run: () => Promise<unknown>) => {
      setError(null)
      run()
        .then(() => reload())
        .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Action failed'))
    },
    [reload, setError],
  )
}

function UsersTab() {
  const [users, setUsers] = useState<UserDto[]>([])
  const [error, setError] = useState<string | null>(null)
  const [drafts, setDrafts] = useState<Record<string, string>>({})
  const [pw, setPw] = useState<Record<string, string>>({})
  // New-user form.
  const [name, setName] = useState('')
  const [pass, setPass] = useState('')
  const [admin, setAdmin] = useState(false)
  const [groups, setGroups] = useState('')

  const load = useCallback(() => {
    listUsers()
      .then(setUsers)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load users'))
  }, [])
  useEffect(load, [load])
  const act = useAction(load, setError)

  const parseGroups = (s: string) =>
    s
      .split(',')
      .map((g) => g.trim())
      .filter(Boolean)

  return (
    <div>
      <h3>Users</h3>
      {error ? <p className="error">{error}</p> : null}
      <table className="placements">
        <thead>
          <tr>
            <th>Username</th>
            <th>Admin</th>
            <th>Groups (comma-separated)</th>
            <th>Password reset</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {users.map((u) => {
            const draft = drafts[u.username] ?? u.groups.join(', ')
            return (
              <tr key={u.username}>
                <td>{u.username}</td>
                <td>
                  <input
                    type="checkbox"
                    checked={u.is_admin}
                    onChange={(e) => act(() => patchUser(u.username, { is_admin: e.target.checked }))}
                  />
                </td>
                <td>
                  <input
                    value={draft}
                    onChange={(e) => setDrafts({ ...drafts, [u.username]: e.target.value })}
                  />
                  <button
                    onClick={() =>
                      act(() => patchUser(u.username, { groups: parseGroups(draft) }))
                    }
                  >
                    Save
                  </button>
                </td>
                <td>
                  <input
                    type="password"
                    placeholder="new password"
                    value={pw[u.username] ?? ''}
                    onChange={(e) => setPw({ ...pw, [u.username]: e.target.value })}
                  />
                  <button
                    disabled={!pw[u.username]}
                    onClick={() =>
                      act(async () => {
                        await patchUser(u.username, { password: pw[u.username] })
                        setPw({ ...pw, [u.username]: '' })
                      })
                    }
                  >
                    Set
                  </button>
                </td>
                <td>
                  <button onClick={() => act(() => deleteUser(u.username))}>Delete</button>
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>

      <h3>Add user</h3>
      <div className="field-row">
        <label>
          Username
          <input value={name} onChange={(e) => setName(e.target.value)} />
        </label>
        <label>
          Password
          <input type="password" value={pass} onChange={(e) => setPass(e.target.value)} />
        </label>
        <label>
          Groups
          <input value={groups} onChange={(e) => setGroups(e.target.value)} />
        </label>
        <label className="check">
          <input type="checkbox" checked={admin} onChange={(e) => setAdmin(e.target.checked)} />
          Administrator
        </label>
      </div>
      <div className="actions">
        <button
          className="primary"
          disabled={!name.trim() || !pass}
          onClick={() =>
            act(async () => {
              await createUser({
                username: name.trim(),
                password: pass,
                is_admin: admin,
                groups: parseGroups(groups),
              })
              setName('')
              setPass('')
              setGroups('')
              setAdmin(false)
            })
          }
        >
          Create user
        </button>
      </div>
    </div>
  )
}

function GroupsTab() {
  const [groups, setGroups] = useState<string[]>([])
  const [error, setError] = useState<string | null>(null)
  const [name, setName] = useState('')

  const load = useCallback(() => {
    listGroups()
      .then(setGroups)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load groups'))
  }, [])
  useEffect(load, [load])
  const act = useAction(load, setError)

  return (
    <div>
      <h3>Groups</h3>
      {error ? <p className="error">{error}</p> : null}
      {groups.length === 0 ? <p className="muted">No groups defined.</p> : null}
      <table className="placements">
        <tbody>
          {groups.map((g) => (
            <tr key={g}>
              <td>{g}</td>
              <td>
                <button onClick={() => act(() => deleteGroup(g))}>Delete</button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <div className="field-row">
        <label>
          New group
          <input value={name} onChange={(e) => setName(e.target.value)} />
        </label>
      </div>
      <div className="actions">
        <button
          className="primary"
          disabled={!name.trim()}
          onClick={() =>
            act(async () => {
              await createGroup(name.trim())
              setName('')
            })
          }
        >
          Create group
        </button>
      </div>
    </div>
  )
}

function CubeGrantsTab() {
  const [grants, setGrants] = useState<CubeGrantDto[]>([])
  const [error, setError] = useState<string | null>(null)
  const [scope, setScope] = useState('')
  const [subjectKind, setSubjectKind] = useState<SubjectKind>('group')
  const [subject, setSubject] = useState('')
  const [level, setLevel] = useState<CubeGrantLevel>('read')

  const load = useCallback(() => {
    listCubeGrants()
      .then(setGrants)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load cube grants'))
  }, [])
  useEffect(load, [load])
  const act = useAction(load, setError)

  return (
    <div>
      <h3>Cube grants</h3>
      <p className="muted">
        Broad access across all cubes plus per-cube exceptions (ADR-0016). The most specific grant
        wins (a per-cube grant overrides a global one), an explicit <em>deny</em> overrides an allow,
        and an admin always has full access. Leave <em>Scope</em> blank for all cubes.
      </p>
      {error ? <p className="error">{error}</p> : null}
      <table className="placements">
        <thead>
          <tr>
            <th>Scope</th>
            <th>Subject</th>
            <th>Access</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {grants.map((g, i) => (
            <tr key={`${g.scope ?? '*'}/${g.subject_kind}/${g.subject}/${g.effect}/${i}`}>
              <td>{g.scope ?? <em>all cubes</em>}</td>
              <td>
                {g.subject_kind}: {g.subject}
              </td>
              <td>{g.effect === 'deny' ? <strong>deny</strong> : g.level}</td>
              <td>
                <button
                  onClick={() =>
                    act(() =>
                      putCubeGrant({
                        scope: g.scope,
                        subject_kind: g.subject_kind,
                        subject: g.subject,
                        level: 'none',
                      }),
                    )
                  }
                >
                  Revoke
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      <h3>Set cube grant</h3>
      <div className="field-row">
        <label>
          Scope (blank for all cubes)
          <input value={scope} onChange={(e) => setScope(e.target.value)} />
        </label>
        <label>
          Subject
          <select
            value={subjectKind}
            onChange={(e) => setSubjectKind(e.target.value as SubjectKind)}
          >
            <option value="user">user</option>
            <option value="group">group</option>
          </select>
        </label>
        <label>
          Subject name
          <input value={subject} onChange={(e) => setSubject(e.target.value)} />
        </label>
        <label>
          Access
          <select value={level} onChange={(e) => setLevel(e.target.value as CubeGrantLevel)}>
            {CUBE_GRANT_LEVELS.map((l) => (
              <option key={l} value={l}>
                {l}
              </option>
            ))}
          </select>
        </label>
      </div>
      <div className="actions">
        <button
          className="primary"
          disabled={!subject.trim()}
          onClick={() =>
            act(async () => {
              await putCubeGrant({
                scope: scope.trim() || undefined,
                subject_kind: subjectKind,
                subject: subject.trim(),
                level,
              })
              setSubject('')
            })
          }
        >
          Apply
        </button>
      </div>
    </div>
  )
}

function ObjectAclTab() {
  const [grants, setGrants] = useState<ObjectGrantDto[]>([])
  const [error, setError] = useState<string | null>(null)
  const [kind, setKind] = useState('cube')
  const [cube, setCube] = useState('')
  const [name, setName] = useState('')
  const [subjectKind, setSubjectKind] = useState<SubjectKind>('user')
  const [subject, setSubject] = useState('')
  const [level, setLevel] = useState<AccessLevel>('read')

  const load = useCallback(() => {
    listObjectAcls()
      .then(setGrants)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load grants'))
  }, [])
  useEffect(load, [load])
  const act = useAction(load, setError)

  return (
    <div>
      <h3>Object access</h3>
      <p className="muted">
        Grants on individual objects (rules, flows, views, subsets, connections, sandboxes) and
        specific-cube allows. For broad cross-cube access and denies, use <em>Cube grants</em>.
        Revoke to set a level of <em>none</em>.
      </p>
      {error ? <p className="error">{error}</p> : null}
      <table className="placements">
        <thead>
          <tr>
            <th>Kind</th>
            <th>Cube</th>
            <th>Name</th>
            <th>Subject</th>
            <th>Level</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {grants.map((g, i) => (
            <tr key={`${g.kind}/${g.cube ?? ''}/${g.name}/${g.subject_kind}/${g.subject}/${i}`}>
              <td>{g.kind}</td>
              <td>{g.cube ?? ''}</td>
              <td>{g.name}</td>
              <td>
                {g.subject_kind}: {g.subject}
              </td>
              <td>{g.level}</td>
              <td>
                <button onClick={() => act(() => putObjectAcl({ ...g, level: 'none' }))}>
                  Revoke
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      <h3>Grant object access</h3>
      <div className="field-row">
        <label>
          Kind
          <select value={kind} onChange={(e) => setKind(e.target.value)}>
            {OBJECT_KINDS.map((k) => (
              <option key={k} value={k}>
                {k}
              </option>
            ))}
          </select>
        </label>
        <label>
          Cube (blank for global)
          <input value={cube} onChange={(e) => setCube(e.target.value)} />
        </label>
        <label>
          Name
          <input value={name} onChange={(e) => setName(e.target.value)} />
        </label>
        <label>
          Subject
          <select value={subjectKind} onChange={(e) => setSubjectKind(e.target.value as SubjectKind)}>
            <option value="user">user</option>
            <option value="group">group</option>
          </select>
        </label>
        <label>
          Subject name
          <input value={subject} onChange={(e) => setSubject(e.target.value)} />
        </label>
        <label>
          Level
          <select value={level} onChange={(e) => setLevel(e.target.value as AccessLevel)}>
            {LEVELS.map((l) => (
              <option key={l} value={l}>
                {l}
              </option>
            ))}
          </select>
        </label>
      </div>
      <div className="actions">
        <button
          className="primary"
          disabled={!name.trim() || !subject.trim()}
          onClick={() =>
            act(async () => {
              await putObjectAcl({
                kind,
                cube: cube.trim() || undefined,
                name: name.trim(),
                subject_kind: subjectKind,
                subject: subject.trim(),
                level,
              })
              setName('')
              setSubject('')
            })
          }
        >
          Grant
        </button>
      </div>
    </div>
  )
}

function ElementAclTab() {
  const [grants, setGrants] = useState<ElementGrantDto[]>([])
  const [error, setError] = useState<string | null>(null)
  const [cube, setCube] = useState('')
  const [dimension, setDimension] = useState('')
  const [element, setElement] = useState('')
  const [subjectKind, setSubjectKind] = useState<SubjectKind>('user')
  const [subject, setSubject] = useState('')
  const [level, setLevel] = useState<AccessLevel>('read')

  const load = useCallback(() => {
    listElementAcls()
      .then(setGrants)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load grants'))
  }, [])
  useEffect(load, [load])
  const act = useAction(load, setError)

  return (
    <div>
      <h3>Element access</h3>
      <p className="muted">
        Granting any subject access to an element restricts it: everyone else (except admins) is then
        denied that member and any total that rolls it up.
      </p>
      {error ? <p className="error">{error}</p> : null}
      <table className="placements">
        <thead>
          <tr>
            <th>Cube</th>
            <th>Dimension</th>
            <th>Element</th>
            <th>Subject</th>
            <th>Level</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {grants.map((g, i) => (
            <tr key={`${g.cube}/${g.dimension}/${g.element}/${g.subject_kind}/${g.subject}/${i}`}>
              <td>{g.cube}</td>
              <td>{g.dimension}</td>
              <td>{g.element}</td>
              <td>
                {g.subject_kind}: {g.subject}
              </td>
              <td>{g.level}</td>
              <td>
                <button onClick={() => act(() => putElementAcl({ ...g, level: 'none' }))}>
                  Revoke
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      <h3>Grant element access</h3>
      <div className="field-row">
        <label>
          Cube
          <input value={cube} onChange={(e) => setCube(e.target.value)} />
        </label>
        <label>
          Dimension
          <input value={dimension} onChange={(e) => setDimension(e.target.value)} />
        </label>
        <label>
          Element
          <input value={element} onChange={(e) => setElement(e.target.value)} />
        </label>
        <label>
          Subject
          <select value={subjectKind} onChange={(e) => setSubjectKind(e.target.value as SubjectKind)}>
            <option value="user">user</option>
            <option value="group">group</option>
          </select>
        </label>
        <label>
          Subject name
          <input value={subject} onChange={(e) => setSubject(e.target.value)} />
        </label>
        <label>
          Level
          <select value={level} onChange={(e) => setLevel(e.target.value as AccessLevel)}>
            {LEVELS.map((l) => (
              <option key={l} value={l}>
                {l}
              </option>
            ))}
          </select>
        </label>
      </div>
      <div className="actions">
        <button
          className="primary"
          disabled={!cube.trim() || !dimension.trim() || !element.trim() || !subject.trim()}
          onClick={() =>
            act(async () => {
              await putElementAcl({
                cube: cube.trim(),
                dimension: dimension.trim(),
                element: element.trim(),
                subject_kind: subjectKind,
                subject: subject.trim(),
                level,
              })
              setElement('')
              setSubject('')
            })
          }
        >
          Grant
        </button>
      </div>
    </div>
  )
}
