import { useCallback, useEffect, useRef, useState } from 'react'

/** The current windowed slice for a virtualized, fixed-row-height scroll region
 * (ADR-0032). Attach `containerRef` + `onScroll` to the scroll container, render
 * only rows `[start, end)`, and pad the list with a top spacer of `offsetTop` and
 * a total scroll height of `totalHeight` so the scrollbar reflects the true size. */
export interface VirtualWindow {
  containerRef: React.RefObject<HTMLDivElement | null>
  onScroll: () => void
  start: number
  end: number
  offsetTop: number
  totalHeight: number
}

/**
 * Tiny in-house row windowing (ADR-0032): render only the rows in (and a small
 * overscan around) the viewport, recycling DOM nodes as the user scrolls, so the
 * rendered node count stays small and constant regardless of `rowCount`. Fixed
 * row height in v1. `enabled=false` renders every row (used below a small
 * threshold so short lists stay plain DOM). The viewport height is measured from
 * the container so the math follows the actual CSS-set scroll-region size.
 */
export function useVirtualRows(opts: {
  rowCount: number
  rowHeight: number
  overscan?: number
  enabled?: boolean
}): VirtualWindow {
  const { rowCount, rowHeight, overscan = 4, enabled = true } = opts
  const containerRef = useRef<HTMLDivElement | null>(null)
  const [scrollTop, setScrollTop] = useState(0)
  const [viewport, setViewport] = useState(0)

  useEffect(() => {
    const el = containerRef.current
    if (!el) return
    const measure = () => setViewport(el.clientHeight)
    measure()
    const ro = new ResizeObserver(measure)
    ro.observe(el)
    return () => ro.disconnect()
  }, [])

  const onScroll = useCallback(() => {
    const el = containerRef.current
    if (el) setScrollTop(el.scrollTop)
  }, [])

  const totalHeight = rowCount * rowHeight
  if (!enabled || viewport === 0) {
    return { containerRef, onScroll, start: 0, end: rowCount, offsetTop: 0, totalHeight }
  }
  const visible = Math.ceil(viewport / rowHeight)
  const start = Math.max(0, Math.floor(scrollTop / rowHeight) - overscan)
  const end = Math.min(rowCount, start + visible + overscan * 2)
  return { containerRef, onScroll, start, end, offsetTop: start * rowHeight, totalHeight }
}
