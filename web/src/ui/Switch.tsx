import * as RS from '@radix-ui/react-switch'
import { useId } from 'react'

/** A labeled on/off toggle (ADR-0020). Wraps the Radix switch so the whole
 * row (label + control) is one accessible, clickable unit. */
export function Switch({
  checked,
  onCheckedChange,
  label,
  description,
  disabled,
}: {
  checked: boolean
  onCheckedChange: (checked: boolean) => void
  label: string
  description?: string
  disabled?: boolean
}) {
  const id = useId()
  return (
    <div className="switch-row">
      <RS.Root
        id={id}
        className="switch"
        checked={checked}
        onCheckedChange={onCheckedChange}
        disabled={disabled}
      >
        <RS.Thumb className="switch__thumb" />
      </RS.Root>
      <label className="switch-row__text" htmlFor={id}>
        <span className="switch-row__label">{label}</span>
        {description ? <span className="switch-row__desc">{description}</span> : null}
      </label>
    </div>
  )
}
