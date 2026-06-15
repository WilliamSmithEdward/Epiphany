import type { ReactNode } from 'react'

/** A teaching empty state: icon, plain-language title + body, and a primary
 * action (ADR-0020 "everything spelled out"). */
export function EmptyState({
  icon,
  title,
  children,
  action,
}: {
  icon?: ReactNode
  title: ReactNode
  children?: ReactNode
  action?: ReactNode
}) {
  return (
    <div className="empty">
      {icon ? (
        <div className="empty__icon" aria-hidden="true">
          {icon}
        </div>
      ) : null}
      <h3 className="empty__title">{title}</h3>
      {children ? <p className="empty__body">{children}</p> : null}
      {action ? <div className="empty__action">{action}</div> : null}
    </div>
  )
}
