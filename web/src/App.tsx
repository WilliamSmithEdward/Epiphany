import { useState } from 'react'
import Login from './components/Login'
import CubeApp from './components/CubeApp'

interface Session {
  username: string
  isAdmin: boolean
}

export default function App() {
  const [session, setSession] = useState<Session | null>(null)

  if (!session) {
    return <Login onLoggedIn={(username, isAdmin) => setSession({ username, isAdmin })} />
  }
  return (
    <CubeApp
      username={session.username}
      isAdmin={session.isAdmin}
      onLogout={() => setSession(null)}
    />
  )
}
