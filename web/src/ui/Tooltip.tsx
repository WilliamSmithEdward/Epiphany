import * as RT from '@radix-ui/react-tooltip'
import type { ReactNode } from 'react'

/** Wrap the app once so tooltips share a hover-delay timer. */
export function TooltipProvider({ children }: { children: ReactNode }) {
  return (
    <RT.Provider delayDuration={150} skipDelayDuration={300}>
      {children}
    </RT.Provider>
  )
}

/** A just-in-time tooltip on any focusable trigger (ADR-0020). */
export function Tooltip({
  content,
  children,
  side = 'top',
}: {
  content: ReactNode
  children: ReactNode
  side?: 'top' | 'right' | 'bottom' | 'left'
}) {
  return (
    <RT.Root>
      <RT.Trigger asChild>{children}</RT.Trigger>
      <RT.Portal>
        <RT.Content className="tooltip" sideOffset={6} side={side}>
          {content}
          <RT.Arrow className="tooltip__arrow" />
        </RT.Content>
      </RT.Portal>
    </RT.Root>
  )
}
