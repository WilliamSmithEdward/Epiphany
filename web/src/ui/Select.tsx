import * as RS from '@radix-ui/react-select'
import type { ReactNode } from 'react'

export interface SelectOption {
  value: string
  label: ReactNode
}

/** A styled, accessible dropdown (Radix Select) over a flat option list. */
export function Select({
  value,
  onValueChange,
  options,
  placeholder = 'Select…',
  ariaLabel,
  disabled,
  className,
}: {
  value: string | undefined
  onValueChange: (value: string) => void
  options: SelectOption[]
  placeholder?: string
  ariaLabel?: string
  disabled?: boolean
  className?: string
}) {
  return (
    <RS.Root value={value} onValueChange={onValueChange} disabled={disabled}>
      <RS.Trigger className={`select ${className ?? ''}`} aria-label={ariaLabel}>
        <RS.Value placeholder={placeholder} />
        <RS.Icon className="select__icon">▾</RS.Icon>
      </RS.Trigger>
      <RS.Portal>
        <RS.Content className="select__content" position="popper" sideOffset={4}>
          <RS.Viewport className="select__viewport">
            {options.map((o) => (
              <RS.Item key={o.value} value={o.value} className="select__item">
                <RS.ItemText>{o.label}</RS.ItemText>
                <RS.ItemIndicator className="select__indicator">✓</RS.ItemIndicator>
              </RS.Item>
            ))}
          </RS.Viewport>
        </RS.Content>
      </RS.Portal>
    </RS.Root>
  )
}
