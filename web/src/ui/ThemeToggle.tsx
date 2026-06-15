import { useState } from 'react'
import { getTheme, toggleTheme, type Theme } from './theme'

/** A compact light/dark toggle for the top bar (ADR-0020). */
export default function ThemeToggle() {
  const [theme, setTheme] = useState<Theme>(getTheme)
  const dark = theme === 'dark'
  return (
    <button
      type="button"
      className="icon-btn"
      aria-label={dark ? 'Switch to light theme' : 'Switch to dark theme'}
      title={dark ? 'Light theme' : 'Dark theme'}
      onClick={() => setTheme(toggleTheme())}
    >
      {dark ? '☀' : '☾'}
    </button>
  )
}
