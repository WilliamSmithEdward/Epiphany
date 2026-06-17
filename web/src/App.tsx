import { useState } from 'react'
import Login from './components/Login'
import CubeApp from './components/CubeApp'
import ChangePassword from './components/ChangePassword'
import ThemeToggle from './ui/ThemeToggle'
import { TooltipProvider, ConfirmProvider } from './ui'

interface Session {
  username: string
  isAdmin: boolean
  /** The account must set a new password before any other access (ADR-0017). */
  mustChange: boolean
}

export default function App() {
  const [session, setSession] = useState<Session | null>(null)

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
        <CubeApp
          username={session.username}
          isAdmin={session.isAdmin}
          onLogout={() => setSession(null)}
        />
      )}
      </ConfirmProvider>
    </TooltipProvider>
  )
}
