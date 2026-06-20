import { useState, type FormEvent } from 'react'
import { login } from '../api/client'
import { notifyExcelHost } from '../host'
import { Button, Field, Input } from '../ui'
import ThemeToggle from '../ui/ThemeToggle'

export default function Login({
  onLoggedIn,
}: {
  onLoggedIn: (username: string, isAdmin: boolean, mustChange: boolean) => void
}) {
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    // Read the field values straight from the form on submit, not only from React
    // state: a browser or password-manager autofill can populate the inputs without
    // firing React's onChange, leaving the controlled state empty even though the
    // fields visibly hold credentials. Reading the DOM (the inputs are name-addressed
    // for this and for autofill) means a submit - whether by Enter or the Sign in
    // button - sends what the user sees, not blanks.
    const form = event.currentTarget
    const u = (form.elements.namedItem('username') as HTMLInputElement | null)?.value ?? username
    const p =
      (form.elements.namedItem('current-password') as HTMLInputElement | null)?.value ?? password
    setBusy(true)
    setError(null)
    try {
      const result = await login(u, p)
      // If embedded in the Excel add-in's WebView2, hand it the token (ADR-0022).
      notifyExcelHost(result.token)
      onLoggedIn(result.user.username, result.user.is_admin, result.user.must_change_password)
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
      <form
        className="login__card"
        onSubmit={(event) => void submit(event)}
        onKeyDown={(event) => {
          // Submit on Enter explicitly. The form already has a type=submit button,
          // so a trusted Enter normally submits implicitly - but some password
          // managers / browsers swallow that implicit submission once a field has
          // been autofilled, leaving Enter doing nothing. requestSubmit() fires the
          // same submit handler + native validation as clicking Sign in. Guarded to
          // a field (not the button, which keeps its own Enter behavior) and not
          // during IME composition; preventDefault avoids a double submit when the
          // implicit one does fire.
          if (
            event.key === 'Enter' &&
            !event.nativeEvent.isComposing &&
            event.target instanceof HTMLInputElement
          ) {
            event.preventDefault()
            event.currentTarget.requestSubmit()
          }
        }}
      >
        <div className="login__brand">
          <div className="login__logo" aria-hidden="true">
            ◆
          </div>
          <h1 className="login__title">Epiphany</h1>
          <p className="login__tagline">Multidimensional planning &amp; analytics</p>
        </div>
        <Field label="Username">
          {(id, a11y) => (
            <Input
              id={id}
              {...a11y}
              name="username"
              value={username}
              autoComplete="username"
              autoFocus
              aria-invalid={error ? true : undefined}
              aria-describedby={error ? 'login-error' : undefined}
              onChange={(e) => setUsername(e.target.value)}
            />
          )}
        </Field>
        <Field label="Password">
          {(id, a11y) => (
            <Input
              id={id}
              {...a11y}
              name="current-password"
              type="password"
              value={password}
              autoComplete="current-password"
              aria-invalid={error ? true : undefined}
              aria-describedby={error ? 'login-error' : undefined}
              onChange={(e) => setPassword(e.target.value)}
            />
          )}
        </Field>
        {error ? (
          <p id="login-error" className="field__msg field__msg--error" role="alert">
            {error}
          </p>
        ) : null}
        <Button variant="primary" type="submit" disabled={busy} className="login__submit">
          {busy ? 'Signing in...' : 'Sign in'}
        </Button>
        <p className="login__hint">
          Use the username and password your administrator gave you.
        </p>
      </form>
    </div>
  )
}
