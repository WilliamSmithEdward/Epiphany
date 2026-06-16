import { useState } from 'react'
import { Button, Card } from '../ui'

const DISMISS_KEY = 'epiphany.welcome.dismissed'

function isDismissed(): boolean {
  try {
    return localStorage.getItem(DISMISS_KEY) === '1'
  } catch {
    return false
  }
}

// A first-run welcome card (W5): a plain-language orientation shown once, until
// dismissed (a localStorage flag). No jargon - it tells a business user what to
// do next, with one extra line for admins. Dismissable; it never reappears.
export default function WelcomeCard({
  username,
  isAdmin,
  hasCubes,
}: {
  username: string
  isAdmin: boolean
  hasCubes: boolean
}) {
  const [hidden, setHidden] = useState(isDismissed)
  if (hidden) return null

  function dismiss() {
    try {
      localStorage.setItem(DISMISS_KEY, '1')
    } catch {
      /* ignore: a session-only dismissal is fine */
    }
    setHidden(true)
  }

  return (
    <Card
      title={`Welcome, ${username}`}
      subtitle="Epiphany is a place to plan and analyze numbers across categories like region, period, and measure."
      actions={
        <Button size="sm" variant="ghost" onClick={dismiss}>
          Got it
        </Button>
      }
    >
      <ul className="welcome-steps">
        <li>
          {hasCubes ? (
            <>
              <strong>Pick a cube</strong> from the sidebar to get started.
            </>
          ) : isAdmin ? (
            <>
              <strong>Create your first cube</strong> in Model to get started.
            </>
          ) : (
            <>
              <strong>Your cubes</strong> appear in the sidebar once an administrator grants you
              access.
            </>
          )}
        </li>
        <li>
          <strong>Open Data</strong> and type a number into a cell; totals recalculate as you go.
        </li>
        <li>
          <strong>Explore Views</strong> to slice the numbers, or <strong>Dimensions</strong> to see
          the categories.
        </li>
        {isAdmin ? (
          <li>
            As an administrator you can also create cubes in <strong>Model</strong>, automate data
            loads in <strong>Flows</strong>, and manage who can see what in{' '}
            <strong>Security &amp; audit</strong>.
          </li>
        ) : null}
      </ul>
    </Card>
  )
}
