// Light/dark theme control (ADR-0020). The theme is a `data-theme` attribute on
// the document root that flips the semantic token layer; we persist the choice
// and fall back to the OS preference. No re-render, no dependency.

export type Theme = 'light' | 'dark'

const STORAGE_KEY = 'epiphany-theme'

/** The persisted theme, or the OS preference when the user has not chosen. */
export function getTheme(): Theme {
  const stored = localStorage.getItem(STORAGE_KEY)
  if (stored === 'light' || stored === 'dark') return stored
  return window.matchMedia?.('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'
}

/** Apply and persist a theme. */
export function setTheme(theme: Theme): void {
  document.documentElement.dataset.theme = theme
  try {
    localStorage.setItem(STORAGE_KEY, theme)
  } catch {
    // Private mode / storage disabled: the attribute still applies for this session.
  }
}

/** Apply the persisted or system theme. Call once before the app renders. */
export function initTheme(): void {
  document.documentElement.dataset.theme = getTheme()
}

/** Flip the theme and return the new value. */
export function toggleTheme(): Theme {
  const next: Theme = getTheme() === 'dark' ? 'light' : 'dark'
  setTheme(next)
  return next
}
