import type { ReactNode } from 'react'
import { cx } from './cx'

/** A surface panel with an optional titled header and actions. */
export function Card({
  title,
  subtitle,
  actions,
  children,
  className,
  padded = true,
}: {
  title?: ReactNode
  subtitle?: ReactNode
  actions?: ReactNode
  children: ReactNode
  className?: string
  padded?: boolean
}) {
  const hasHead = title != null || actions != null
  return (
    <section className={cx('card2', className)}>
      {hasHead ? (
        <header className="card2__head">
          <div className="card2__titles">
            {title ? <h3 className="card2__title">{title}</h3> : null}
            {subtitle ? <p className="card2__subtitle">{subtitle}</p> : null}
          </div>
          {actions ? <div className="card2__actions">{actions}</div> : null}
        </header>
      ) : null}
      <div className={cx('card2__body', !padded && 'card2__body--flush')}>{children}</div>
    </section>
  )
}
