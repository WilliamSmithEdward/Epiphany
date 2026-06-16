import { useState, type FormEvent } from 'react'
import { login } from '../api/client'
import { notifyExcelHost } from '../host'
import { Button, Field, Input } from '../ui'
import ThemeToggle from '../ui/ThemeToggle'

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
      // If embedded in the Excel add-in's WebView2, hand it the token (ADR-0022).
      notifyExcelHost(result.token)
      onLoggedIn(result.user.username, result.user.is_admin)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Sign-in failed')
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="login">
      <div className="login__corner">
        <ThemeToggle />
      </div>
      <form className="login__card" onSubmit={(event) => void submit(event)}>
        <div className="login__brand">
          <div className="login__logo" aria-hidden="true">
            ◆
          </div>
          <h1 className="login__title">Epiphany</h1>
          <p className="login__tagline">Multidimensional planning &amp; analytics</p>
        </div>
        <Field label="Username">
          {(id) => (
            <Input
              id={id}
              value={username}
              autoComplete="username"
              autoFocus
              onChange={(e) => setUsername(e.target.value)}
            />
          )}
        </Field>
        <Field label="Password">
          {(id) => (
            <Input
              id={id}
              type="password"
              value={password}
              autoComplete="current-password"
              onChange={(e) => setPassword(e.target.value)}
            />
          )}
        </Field>
        {error ? (
          <p className="field__msg field__msg--error" role="alert">
            {error}
          </p>
        ) : null}
        <Button variant="primary" type="submit" disabled={busy} className="login__submit">
          {busy ? 'Signing in…' : 'Sign in'}
        </Button>
        <p className="login__hint">
          First time? On first run the server writes a one-time admin password to{' '}
          <code>data/server/admin-password.txt</code> and loads a demo cube you can explore right
          away.
        </p>
      </form>
    </div>
  )
}
