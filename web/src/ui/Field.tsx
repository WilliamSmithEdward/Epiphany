import {
  forwardRef,
  useId,
  type InputHTMLAttributes,
  type ReactNode,
  type TextareaHTMLAttributes,
} from 'react'
import { cx } from './cx'

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement>>(
  function Input({ className, ...rest }, ref) {
    return <input ref={ref} className={cx('input', className)} {...rest} />
  },
)

export const Textarea = forwardRef<HTMLTextAreaElement, TextareaHTMLAttributes<HTMLTextAreaElement>>(
  function Textarea({ className, ...rest }, ref) {
    return <textarea ref={ref} className={cx('input', 'textarea', className)} {...rest} />
  },
)

/** A labelled form field with optional hint and error, wired for accessibility. */
export function Field({
  label,
  hint,
  error,
  children,
  className,
}: {
  label: ReactNode
  hint?: ReactNode
  error?: ReactNode
  /**
   * Render-prop receiving the generated id plus the accessibility props
   * (`aria-describedby` / `aria-invalid`) to spread onto the control so the
   * hint/error is programmatically associated and invalid state is exposed.
   */
  children: (
    id: string,
    a11y: { 'aria-describedby'?: string; 'aria-invalid'?: boolean },
  ) => ReactNode
  className?: string
}) {
  const id = useId()
  const msgId = `${id}-msg`
  const a11y = {
    'aria-describedby': error || hint ? msgId : undefined,
    'aria-invalid': error ? true : undefined,
  }
  return (
    <div className={cx('field', className)}>
      <label className="field__label" htmlFor={id}>
        {label}
      </label>
      {children(id, a11y)}
      {error ? (
        <p className="field__msg field__msg--error" id={msgId} role="alert">
          {error}
        </p>
      ) : hint ? (
        <p className="field__msg field__msg--hint" id={msgId}>
          {hint}
        </p>
      ) : null}
    </div>
  )
}
