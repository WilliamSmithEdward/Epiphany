import * as RD from '@radix-ui/react-dialog'
import type { ReactNode } from 'react'

/** A modal dialog with managed focus/escape/scroll-lock (Radix), token-styled. */
export function Dialog({
  open,
  onOpenChange,
  title,
  description,
  children,
  footer,
  size = 'md',
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  title: ReactNode
  description?: ReactNode
  children: ReactNode
  footer?: ReactNode
  size?: 'sm' | 'md' | 'lg' | 'xl'
}) {
  return (
    <RD.Root open={open} onOpenChange={onOpenChange}>
      <RD.Portal>
        <RD.Overlay className="dialog__overlay" />
        <RD.Content className={`dialog dialog--${size}`}>
          <div className="dialog__head">
            <RD.Title className="dialog__title">{title}</RD.Title>
            <RD.Close className="icon-btn" aria-label="Close">
              ✕
            </RD.Close>
          </div>
          {description ? (
            <RD.Description className="dialog__desc">{description}</RD.Description>
          ) : null}
          <div className="dialog__body">{children}</div>
          {footer ? <div className="dialog__footer">{footer}</div> : null}
        </RD.Content>
      </RD.Portal>
    </RD.Root>
  )
}
