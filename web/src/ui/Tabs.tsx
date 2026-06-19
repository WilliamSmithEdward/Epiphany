import * as RT from '@radix-ui/react-tabs'
import type { ReactNode } from 'react'

export interface TabItem {
  value: string
  label: ReactNode
  /** Optional trailing badge/count. */
  badge?: ReactNode
}

/** A horizontal tab strip + panels (Radix Tabs), token-styled, keyboard-accessible. */
export function Tabs({
  value,
  onValueChange,
  items,
  children,
}: {
  value: string
  onValueChange: (value: string) => void
  items: TabItem[]
  /** Panels: one <TabPanel value=...> per tab. */
  children: ReactNode
}) {
  return (
    <RT.Root value={value} onValueChange={onValueChange} className="tabs">
      <RT.List className="tabs__list">
        {items.map((it) => (
          <RT.Trigger key={it.value} value={it.value} className="tabs__trigger">
            {it.label}
            {it.badge != null ? <span className="tabs__badge">{it.badge}</span> : null}
          </RT.Trigger>
        ))}
      </RT.List>
      {children}
    </RT.Root>
  )
}

export function TabPanel({ value, children }: { value: string; children: ReactNode }) {
  return (
    <RT.Content value={value} className="tabs__panel">
      {children}
    </RT.Content>
  )
}
