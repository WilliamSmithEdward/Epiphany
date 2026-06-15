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
  /** Render-prop receiving the generated id to bind the control's `id`. */
  children: (id: string) => ReactNode
  className?: string
}) {
  const id = useId()
  const hintId = `${id}-hint`
  return (
    <div className={cx('field', className)}>
      <label className="field__label" htmlFor={id}>
        {label}
      </label>
      {children(id)}
      {error ? (
        <p className="field__msg field__msg--error" id={hintId}>
          {error}
        </p>
      ) : hint ? (
        <p className="field__msg field__msg--hint" id={hintId}>
          {hint}
        </p>
      ) : null}
    </div>
  )
}
