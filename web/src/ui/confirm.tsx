import { createContext, useCallback, useContext, useRef, useState, type ReactNode } from 'react'
import { Dialog } from './Dialog'
import { Button } from './Button'

export interface ConfirmOptions {
  title: string
  body?: ReactNode
  confirmLabel?: string
  /** When true, the confirm button uses the destructive variant. */
  danger?: boolean
}

type ConfirmFn = (opts: ConfirmOptions) => Promise<boolean>

const ConfirmContext = createContext<ConfirmFn | null>(null)

interface PendingRequest extends ConfirmOptions {
  resolve: (ok: boolean) => void
}

/**
 * Provides a promise-based confirmation dialog built on the vendored Dialog
 * primitive. Mount once around the authenticated app tree, then call
 * `useConfirm()` and `await` it before any destructive action.
 */
export function ConfirmProvider({ children }: { children: ReactNode }) {
  const [pending, setPending] = useState<PendingRequest | null>(null)
  // Keep the latest resolver so overlay/escape close can resolve false safely.
  const pendingRef = useRef<PendingRequest | null>(null)
  pendingRef.current = pending

  const confirm = useCallback<ConfirmFn>((opts) => {
    return new Promise<boolean>((resolve) => {
      setPending({ ...opts, resolve })
    })
  }, [])

  const settle = useCallback((ok: boolean) => {
    const req = pendingRef.current
    if (req) {
      req.resolve(ok)
    }
    setPending(null)
  }, [])

  return (
    <ConfirmContext.Provider value={confirm}>
      {children}
      <Dialog
        open={pending != null}
        onOpenChange={(open) => {
          if (!open) settle(false)
        }}
        title={pending?.title ?? ''}
        size="sm"
        footer={
          <>
            <Button variant="ghost" onClick={() => settle(false)}>
              Cancel
            </Button>
            <Button
              variant={pending?.danger ? 'danger' : 'primary'}
              onClick={() => settle(true)}
            >
              {pending?.confirmLabel ?? 'Confirm'}
            </Button>
          </>
        }
      >
        {pending?.body ?? null}
      </Dialog>
    </ConfirmContext.Provider>
  )
}

/** Returns an async confirm function; resolves true if the user confirms. */
// eslint-disable-next-line react-refresh/only-export-components
export function useConfirm(): ConfirmFn {
  const ctx = useContext(ConfirmContext)
  if (!ctx) {
    throw new Error('useConfirm must be used within a ConfirmProvider')
  }
  return ctx
}
