import { useState } from 'react'
import type { FormEvent } from 'react'
import { login } from '../api/client'

export default function Login({
  onLoggedIn,
}: {
  onLoggedIn: (username: string, isAdmin: boolean) => void
}) {
  const [username, setUsername] = useState('admin')
  const [password, setPassword] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  async function submit(event: FormEvent) {
    event.preventDefault()
    setBusy(true)
    setError(null)
    try {
      const result = await login(username, password)
      onLoggedIn(result.user.username, result.user.is_admin)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Sign-in failed')
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="login">
      <form className="card" onSubmit={(event) => void submit(event)}>
        <h1>Epiphany</h1>
        <label>
          Username
          <input value={username} onChange={(event) => setUsername(event.target.value)} autoFocus />
        </label>
        <label>
          Password
          <input
            type="password"
            value={password}
            onChange={(event) => setPassword(event.target.value)}
          />
        </label>
        {error ? <p className="error">{error}</p> : null}
        <button type="submit" disabled={busy}>
          {busy ? 'Signing in...' : 'Sign in'}
        </button>
      </form>
    </div>
  )
}
