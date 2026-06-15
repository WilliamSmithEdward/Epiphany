import * as DM from '@radix-ui/react-dropdown-menu'
import type { ReactNode } from 'react'

/** A dropdown menu (Radix), token-styled and keyboard-accessible. */
export function Menu({
  trigger,
  children,
  align = 'end',
}: {
  trigger: ReactNode
  children: ReactNode
  align?: 'start' | 'center' | 'end'
}) {
  return (
    <DM.Root>
      <DM.Trigger asChild>{trigger}</DM.Trigger>
      <DM.Portal>
        <DM.Content className="menu" align={align} sideOffset={6}>
          {children}
        </DM.Content>
      </DM.Portal>
    </DM.Root>
  )
}

export function MenuItem({
  children,
  onSelect,
  danger,
}: {
  children: ReactNode
  onSelect?: () => void
  danger?: boolean
}) {
  return (
    <DM.Item className={danger ? 'menu__item menu__item--danger' : 'menu__item'} onSelect={onSelect}>
      {children}
    </DM.Item>
  )
}

export function MenuLabel({ children }: { children: ReactNode }) {
  return <DM.Label className="menu__label">{children}</DM.Label>
}

export function MenuSeparator() {
  return <DM.Separator className="menu__sep" />
}
