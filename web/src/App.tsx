import { useEffect, useState } from 'react'
import Login from './components/Login'
import CubeApp from './components/CubeApp'
import ChangePassword from './components/ChangePassword'
import ErrorBoundary from './components/ErrorBoundary'
import ThemeToggle from './ui/ThemeToggle'
import { TooltipProvider, ConfirmProvider } from './ui'
import { ApiError, getMe, setSessionExpiredHandler } from './api/client'

interface Session {
  username: string
  isAdmin: boolean
  /** The account must set a new password before any other access (ADR-0017). */
  mustChange: boolean
}

export default function App() {
  const [session, setSession] = useState<Session | null>(null)
  // True until the one-time session bootstrap resolves. The logged-in state is
  // in-memory only, so on a page reload we ask the server who we are: the
  // HttpOnly session cookie (set at login, surviving an in-tab reload)
  // authenticates GET /auth/me even though the in-memory bearer token is gone.
  // While checking we render a neutral splash, never <Login/>, so a returning
  // user does not see a login flash before /auth/me resolves.
  const [checking, setChecking] = useState(true)

  useEffect(() => {
    let cancelled = false
    // Idempotent GET, so React StrictMode's double-invoke in dev is harmless.
    // A 401 is a genuine "no session" (show Login). A network blip or a 5xx
    // during load must NOT bounce a user with a live cookie session to Login, so
    // retry getMe once for those; only after the retry also fails as non-401 do
    // we fall back to Login (the safe default) rather than risk a wedged splash.
    const bootstrap = (retry: boolean) => {
      getMe()
        .then((me) => {
          if (cancelled) return
          setSession({
            username: me.username,
            isAdmin: me.is_admin,
            mustChange: me.must_change_password,
          })
          setChecking(false)
        })
        .catch((err: unknown) => {
          if (cancelled) return
          const is401 = err instanceof ApiError && err.status === 401
          if (!is401 && retry) {
            // Transient network/5xx on first try: retry once before giving up.
            bootstrap(false)
            return
          }
          // No active session (401), or a persistent failure after one retry:
          // leave session null and show Login with no error banner.
          setChecking(false)
        })
    }
    bootstrap(true)
    return () => {
      cancelled = true
    }
  }, [])

  // When a request hits a dead session (expired mid-use), drop back to Login in
  // place instead of leaving a zombie UI whose every panel shows a local
  // "session expired" error. Registered once; cleared on unmount.
  useEffect(() => {
    setSessionExpiredHandler(() => setSession(null))
    return () => setSessionExpiredHandler(null)
  }, [])

  if (checking) {
    // Neutral splash while the session check is in flight (not <Login/>).
    return (
      <TooltipProvider>
        <div className="login">
          <div className="login__corner">
            <ThemeToggle />
          </div>
          <div className="login__card">
            <div className="login__brand">
              <div className="login__logo" aria-hidden="true">
                ◆
              </div>
              <h1 className="login__title">Epiphany</h1>
            </div>
          </div>
        </div>
      </TooltipProvider>
    )
  }

  return (
    <TooltipProvider>
      <ConfirmProvider>
      {!session ? (
        <Login
          onLoggedIn={(username, isAdmin, mustChange) =>
            setSession({ username, isAdmin, mustChange })
          }
        />
      ) : session.mustChange ? (
        <div className="login">
          <div className="login__corner">
            <ThemeToggle />
          </div>
          <div className="login__card">
            <div className="login__brand">
              <div className="login__logo" aria-hidden="true">
                ◆
              </div>
              <h1 className="login__title">Choose a new password</h1>
              <p className="login__tagline">Set your own password before you continue.</p>
            </div>
            <ChangePassword
              submitLabel="Set password and continue"
              onDone={() => setSession({ ...session, mustChange: false })}
            />
          </div>
        </div>
      ) : (
        // Wrap the authenticated app so a crash inside it shows a recoverable
        // fallback (with a Reload button) instead of React unmounting the whole
        // root and blanking the document. Login / change-password live outside
        // this boundary (each carries its own ThemeToggle), so an unauthenticated
        // user always has a working chrome to recover through.
        <ErrorBoundary>
          <CubeApp
            username={session.username}
            isAdmin={session.isAdmin}
            onLogout={() => setSession(null)}
          />
        </ErrorBoundary>
      )}
      </ConfirmProvider>
    </TooltipProvider>
  )
}
