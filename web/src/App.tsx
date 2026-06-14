import { useState } from 'react'
import Login from './components/Login'
import CubeApp from './components/CubeApp'

export default function App() {
  const [username, setUsername] = useState<string | null>(null)

  if (!username) {
    return <Login onLoggedIn={setUsername} />
  }
  return <CubeApp username={username} onLogout={() => setUsername(null)} />
}
