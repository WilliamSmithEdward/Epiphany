import { useState } from 'react'
import Login from './components/Login'
import CubeApp from './components/CubeApp'
import { TooltipProvider } from './ui'

interface Session {
  username: string
  isAdmin: boolean
}

export default function App() {
  const [session, setSession] = useState<Session | null>(null)

  return (
    <TooltipProvider>
      {!session ? (
        <Login onLoggedIn={(username, isAdmin) => setSession({ username, isAdmin })} />
      ) : (
        <CubeApp
          username={session.username}
          isAdmin={session.isAdmin}
          onLogout={() => setSession(null)}
        />
      )}
    </TooltipProvider>
  )
}
