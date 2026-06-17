import { useCallback, useEffect, useState } from 'react'
import {
  createGroup,
  createUser,
  deleteGroup,
  deleteUser,
  listElementAcls,
  listGroups,
  listGrants,
  listUsers,
  patchUser,
  putElementAcl,
  resetUserPassword,
  setGrant,
  type AccessLevel,
  type ElementGrantDto,
  type GrantDto,
  type GrantKind,
  type SubjectKind,
  type UserDto,
} from '../api/client'
import AuditViewer from './AuditViewer'
import { useConfirm } from '../ui'

type Tab = 'users' | 'groups' | 'roles' | 'elements' | 'audit'

/** The grantable object kinds (ADR-0023), with plain-language labels. */
const GRANT_KINDS: { value: GrantKind; label: string }[] = [
  { value: 'cube', label: 'Cube (data + lifecycle)' },
  { value: 'dimension', label: 'Dimension (members, hierarchies, attributes)' },
  { value: 'rule', label: 'Rule' },
  { value: 'flow', label: 'Flow' },
  { value: 'view', label: 'View' },
  { value: 'subset', label: 'Subset' },
  { value: 'job', label: 'Schedule (job)' },
  { value: 'connection', label: 'Connection' },
  { value: 'sandbox', label: 'Sandbox' },
]

/** One-click role presets (ADR-0023): each fills the grant form. */
const ROLE_PRESETS: { label: string; scope: 'global' | 'cube'; kind: GrantKind; level: AccessLevel }[] = [
  { label: 'Data entry (write cells)', scope: 'cube', kind: 'cube', level: 'write' },
  { label: 'Flow author (build + run flows)', scope: 'global', kind: 'flow', level: 'write' },
  { label: 'Modeler: dimensions', scope: 'cube', kind: 'dimension', level: 'write' },
  { label: 'Modeler: rules', scope: 'cube', kind: 'rule', level: 'write' },
  { label: 'Cube manager (create/admin cubes)', scope: 'global', kind: 'cube', level: 'admin' },
]

const LEVELS: AccessLevel[] = ['read', 'write', 'admin']

// The server-global security console (ADR-0023 + ADR-0010): users, groups, the
// modular per-object-kind grants (roles), element access, and the audit log.
// Admin only; the topbar hides the entry point for everyone else and every route
// is server-gated regardless.
export default function SecurityWorkspace() {
  const [tab, setTab] = useState<Tab>('users')
  return (
    <div>
      <div className="tabs">
        <button
          className={tab === 'users' ? 'active' : ''}
          aria-current={tab === 'users' ? 'true' : undefined}
          onClick={() => setTab('users')}
        >
          Users
        </button>
        <button
          className={tab === 'groups' ? 'active' : ''}
          aria-current={tab === 'groups' ? 'true' : undefined}
          onClick={() => setTab('groups')}
        >
          Groups
        </button>
        <button
          className={tab === 'roles' ? 'active' : ''}
          aria-current={tab === 'roles' ? 'true' : undefined}
          onClick={() => setTab('roles')}
        >
          Roles
        </button>
        <button
          className={tab === 'elements' ? 'active' : ''}
          aria-current={tab === 'elements' ? 'true' : undefined}
          onClick={() => setTab('elements')}
        >
          Element access
        </button>
        <button
          className={tab === 'audit' ? 'active' : ''}
          aria-current={tab === 'audit' ? 'true' : undefined}
          onClick={() => setTab('audit')}
        >
          Audit
        </button>
      </div>
      {tab === 'users' ? <UsersTab /> : null}
      {tab === 'groups' ? <GroupsTab /> : null}
      {tab === 'roles' ? <RolesTab /> : null}
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
  const confirm = useConfirm()
  const [users, setUsers] = useState<UserDto[]>([])
  const [error, setError] = useState<string | null>(null)
  const [drafts, setDrafts] = useState<Record<string, string>>({})
  const [pw, setPw] = useState<Record<string, string>>({})
  // The one-time temporary password from the most recent admin reset.
  const [temp, setTemp] = useState<{ user: string; password: string } | null>(null)
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

  // Reset a user to a system-generated temporary password; show it once.
  async function resetTemp(username: string) {
    setError(null)
    try {
      const r = await resetUserPassword(username)
      setTemp({ user: r.username, password: r.temp_password })
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Reset failed')
    }
  }

  return (
    <div>
      <h3>Users</h3>
      {error ? <p className="error" role="alert">{error}</p> : null}
      {temp ? (
        <p className="banner" role="status">
          Temporary password for <strong>{temp.user}</strong>: <code>{temp.password}</code>. Share
          it securely. It will not be shown again, and {temp.user} must choose a new password at
          next sign-in.{' '}
          <button onClick={() => void navigator.clipboard?.writeText(temp.password)}>Copy</button>{' '}
          <button onClick={() => setTemp(null)}>Dismiss</button>
        </p>
      ) : null}
      <table className="placements">
        <caption className="sr-only">Users</caption>
        <thead>
          <tr>
            <th scope="col">Username</th>
            <th scope="col">Admin</th>
            <th scope="col">Groups (comma-separated)</th>
            <th scope="col">Password reset</th>
            <th scope="col">
              <span className="sr-only">Actions</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {users.map((u) => {
            const draft = drafts[u.username] ?? u.groups.join(', ')
            return (
              <tr key={u.username}>
                <th scope="row">{u.username}</th>
                <td>
                  <input
                    type="checkbox"
                    aria-label={`Administrator: ${u.username}`}
                    checked={u.is_admin}
                    onChange={(e) => {
                      const next = e.target.checked
                      void confirm({
                        title: next ? 'Grant administrator' : 'Revoke administrator',
                        body: next
                          ? `Make "${u.username}" an administrator? They will gain full control over the server.`
                          : `Remove administrator rights from "${u.username}"?`,
                        confirmLabel: next ? 'Make admin' : 'Remove admin',
                        danger: !next,
                      }).then((ok) => {
                        if (ok) act(() => patchUser(u.username, { is_admin: next }))
                      })
                    }}
                  />
                </td>
                <td>
                  <input
                    aria-label={`Groups for ${u.username}`}
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
                    aria-label={`New password for ${u.username}`}
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
                  <button onClick={() => void resetTemp(u.username)}>Reset to temp</button>
                </td>
                <td>
                  <button
                    onClick={() =>
                      void confirm({
                        title: 'Delete user',
                        body: `Permanently delete user "${u.username}"? This cannot be undone.`,
                        confirmLabel: 'Delete',
                        danger: true,
                      }).then((ok) => {
                        if (ok) act(() => deleteUser(u.username))
                      })
                    }
                  >
                    Delete
                  </button>
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
  const confirm = useConfirm()
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
      {error ? <p className="error" role="alert">{error}</p> : null}
      {groups.length === 0 ? <p className="muted">No groups defined.</p> : null}
      <table className="placements">
        <tbody>
          {groups.map((g) => (
            <tr key={g}>
              <th scope="row">{g}</th>
              <td>
                <button
                  onClick={() =>
                    void confirm({
                      title: 'Delete group',
                      body: `Delete group "${g}"? Members will lose any access granted through it. This cannot be undone.`,
                      confirmLabel: 'Delete',
                      danger: true,
                    }).then((ok) => {
                      if (ok) act(() => deleteGroup(g))
                    })
                  }
                >
                  Delete
                </button>
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

function RolesTab() {
  const confirm = useConfirm()
  const [grants, setGrants] = useState<GrantDto[]>([])
  const [error, setError] = useState<string | null>(null)
  const [subjectKind, setSubjectKind] = useState<SubjectKind>('group')
  const [subject, setSubject] = useState('')
  const [scope, setScope] = useState<'global' | 'cube'>('global')
  const [cube, setCube] = useState('')
  const [kind, setKind] = useState<GrantKind>('flow')
  const [level, setLevel] = useState<AccessLevel>('write')

  const load = useCallback(() => {
    listGrants()
      .then(setGrants)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : 'Failed to load roles'))
  }, [])
  useEffect(load, [load])
  const act = useAction(load, setError)

  return (
    <div>
      <h3>Roles (per-object-kind grants)</h3>
      <p className="muted">
        Grant a user or group a level on one object kind, globally or for one cube (ADR-0023). This
        is how you separate a modeler from a data-entry user: e.g. a group with <em>Flow: write</em>{' '}
        can build and run flows but cannot write cells or edit dimensions. An admin always has full
        access; absence of a grant means no access.
      </p>
      {error ? <p className="error" role="alert">{error}</p> : null}
      <table className="placements">
        <caption className="sr-only">Role grants</caption>
        <thead>
          <tr>
            <th scope="col">Subject</th>
            <th scope="col">Scope</th>
            <th scope="col">Kind</th>
            <th scope="col">Level</th>
            <th scope="col">
              <span className="sr-only">Actions</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {grants.map((g, i) => (
            <tr key={`${g.subject_kind}/${g.subject}/${g.scope}/${g.cube ?? '*'}/${g.kind}/${i}`}>
              <td>
                {g.subject_kind}: {g.subject}
              </td>
              <td>{g.scope === 'global' ? <em>all cubes</em> : g.cube}</td>
              <td>{g.kind}</td>
              <td>{g.level}</td>
              <td>
                <button
                  onClick={() =>
                    void confirm({
                      title: 'Revoke role',
                      body: `Revoke ${g.subject_kind}: ${g.subject} → ${g.kind} on ${g.scope === 'global' ? 'all cubes' : g.cube}?`,
                      confirmLabel: 'Revoke',
                      danger: true,
                    }).then((ok) => {
                      if (ok) act(() => setGrant({ ...g, level: 'none' as AccessLevel }))
                    })
                  }
                >
                  Revoke
                </button>
              </td>
            </tr>
          ))}
          {grants.length === 0 ? (
            <tr>
              <td colSpan={5} className="muted">
                No role grants yet.
              </td>
            </tr>
          ) : null}
        </tbody>
      </table>

      <h3>Grant a role</h3>
      <p className="muted">Quick presets (fill the form, then set the subject and Apply):</p>
      <div className="actions">
        {ROLE_PRESETS.map((p) => (
          <button
            key={p.label}
            onClick={() => {
              setScope(p.scope)
              setKind(p.kind)
              setLevel(p.level)
            }}
          >
            {p.label}
          </button>
        ))}
      </div>
      <div className="field-row">
        <label>
          Subject
          <select
            value={subjectKind}
            onChange={(e) => setSubjectKind(e.target.value as SubjectKind)}
          >
            <option value="group">group</option>
            <option value="user">user</option>
          </select>
        </label>
        <label>
          Subject name
          <input value={subject} onChange={(e) => setSubject(e.target.value)} />
        </label>
        <label>
          Scope
          <select value={scope} onChange={(e) => setScope(e.target.value as 'global' | 'cube')}>
            <option value="global">all cubes</option>
            <option value="cube">one cube</option>
          </select>
        </label>
        {scope === 'cube' ? (
          <label>
            Cube
            <input value={cube} onChange={(e) => setCube(e.target.value)} />
          </label>
        ) : null}
        <label>
          Kind
          <select value={kind} onChange={(e) => setKind(e.target.value as GrantKind)}>
            {GRANT_KINDS.map((k) => (
              <option key={k.value} value={k.value}>
                {k.label}
              </option>
            ))}
          </select>
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
          disabled={!subject.trim() || (scope === 'cube' && !cube.trim())}
          onClick={() =>
            act(async () => {
              await setGrant({
                subject_kind: subjectKind,
                subject: subject.trim(),
                scope,
                cube: scope === 'cube' ? cube.trim() : undefined,
                kind,
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

function ElementAclTab() {
  const confirm = useConfirm()
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
      {error ? <p className="error" role="alert">{error}</p> : null}
      <table className="placements">
        <caption className="sr-only">Element access</caption>
        <thead>
          <tr>
            <th scope="col">Cube</th>
            <th scope="col">Dimension</th>
            <th scope="col">Element</th>
            <th scope="col">Subject</th>
            <th scope="col">Level</th>
            <th scope="col">
              <span className="sr-only">Actions</span>
            </th>
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
                <button
                  onClick={() =>
                    void confirm({
                      title: 'Revoke element access',
                      body: `Revoke ${g.subject_kind}: ${g.subject} access to ${g.cube}/${g.dimension}/${g.element}? Everyone else will remain restricted from this member.`,
                      confirmLabel: 'Revoke',
                      danger: true,
                    }).then((ok) => {
                      if (ok) act(() => putElementAcl({ ...g, level: 'none' }))
                    })
                  }
                >
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
