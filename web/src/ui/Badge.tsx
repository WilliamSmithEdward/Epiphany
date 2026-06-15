import type { ReactNode } from 'react'
import { cx } from './cx'

type Tone = 'neutral' | 'info' | 'success' | 'warning' | 'danger'

/** A small status pill, color-coded by tone (ADR-0020 status badges). */
export function Badge({
  tone = 'neutral',
  dot = false,
  children,
  className,
}: {
  tone?: Tone
  dot?: boolean
  children: ReactNode
  className?: string
}) {
  return (
    <span className={cx('badge', `badge--${tone}`, className)}>
      {dot ? <span className="badge__dot" aria-hidden="true" /> : null}
      {children}
    </span>
  )
}
