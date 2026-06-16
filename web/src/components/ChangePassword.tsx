import { useState, type FormEvent } from 'react'
import { changePassword } from '../api/client'
import { Button, Field, Input } from '../ui'

/**
 * Change-password form. Used both for the forced first-login rotation (rendered
 * as a full-screen gate by App) and for a voluntary change from the account menu
 * (rendered inside a dialog by CubeApp). The caller supplies the surrounding
 * chrome; this renders only the fields, validation, and submit.
 */
export default function ChangePassword({
  onDone,
  onCancel,
  submitLabel = 'Update password',
}: {
  onDone: () => void
  onCancel?: () => void
  submitLabel?: string
}) {
  const [current, setCurrent] = useState('')
  const [next, setNext] = useState('')
  const [confirm, setConfirm] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  async function submit(event: FormEvent) {
    event.preventDefault()
    setError(null)
    if (next !== confirm) {
      setError('The new password and its confirmation do not match.')
      return
    }
    setBusy(true)
    try {
      await changePassword(current, next)
      onDone()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not change the password.')
    } finally {
      setBusy(false)
    }
  }

  return (
    <form className="pw-form" onSubmit={(event) => void submit(event)}>
      <Field label="Current password">
        {(id) => (
          <Input
            id={id}
            type="password"
            value={current}
            autoComplete="current-password"
            autoFocus
            onChange={(e) => setCurrent(e.target.value)}
          />
        )}
      </Field>
      <Field label="New password" hint="At least 12 characters; avoid common passwords.">
        {(id) => (
          <Input
            id={id}
            type="password"
            value={next}
            autoComplete="new-password"
            onChange={(e) => setNext(e.target.value)}
          />
        )}
      </Field>
      <Field label="Confirm new password">
        {(id) => (
          <Input
            id={id}
            type="password"
            value={confirm}
            autoComplete="new-password"
            onChange={(e) => setConfirm(e.target.value)}
          />
        )}
      </Field>
      {error ? (
        <p className="field__msg field__msg--error" role="alert">
          {error}
        </p>
      ) : null}
      <div className="pw-form__actions">
        {onCancel ? (
          <Button variant="ghost" type="button" onClick={onCancel} disabled={busy}>
            Cancel
          </Button>
        ) : null}
        <Button variant="primary" type="submit" disabled={busy}>
          {busy ? 'Updating…' : submitLabel}
        </Button>
      </div>
    </form>
  )
}
