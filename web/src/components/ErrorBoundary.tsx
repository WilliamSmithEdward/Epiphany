import { Component, type ErrorInfo, type ReactNode } from 'react'

/** A render-time crash barrier (there is no other one in the app). React 19
 * unmounts the whole root on an uncaught render throw, and the async try/catch
 * inside panes does not cover synchronous render errors, so without this a
 * single bad render blanks the entire document. Wrapping a subtree here turns
 * that into a calm, local fallback the user can recover from. */
export default class ErrorBoundary extends Component<
  {
    children: ReactNode
    /** When this value changes, the caught error is cleared so the subtree can
     * re-render (e.g. switching/closing the crashed tab recovers). */
    resetKey?: unknown
  },
  { error: Error | null }
> {
  state: { error: Error | null } = { error: null }

  static getDerivedStateFromError(error: Error): { error: Error | null } {
    return { error }
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    // The fallback shows only error.message; log the full error + component
    // stack so a developer can diagnose the crash from the console.
    console.error('ErrorBoundary caught a render error', error, info)
  }

  componentDidUpdate(prev: {
    children: ReactNode
    resetKey?: unknown
  }): void {
    // Clear the error when the reset key changes (e.g. the active tab changed),
    // so a previously crashed pane gets a fresh mount instead of staying stuck
    // on the fallback after the user has navigated away.
    if (this.state.error !== null && prev.resetKey !== this.props.resetKey) {
      this.setState({ error: null })
    }
  }

  render(): ReactNode {
    if (this.state.error !== null) {
      return (
        <div className="empty" role="alert">
          <div className="empty__icon" aria-hidden="true">
            !
          </div>
          <h3 className="empty__title">Something went wrong here</h3>
          <p className="empty__body">
            This part of the app hit an unexpected error and stopped. Your other
            open objects are unaffected. Reload the page to recover, or switch to
            another tab.
          </p>
          <p className="muted">{this.state.error.message}</p>
          <div className="empty__action">
            <button
              type="button"
              className="btn btn--primary btn--md"
              onClick={() => location.reload()}
            >
              Reload
            </button>
          </div>
        </div>
      )
    }
    return this.props.children
  }
}
