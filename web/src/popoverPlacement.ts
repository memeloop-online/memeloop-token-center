export interface ViewportBounds { left: number; top: number; width: number; height: number }
export interface AnchorBounds { left: number; top: number; bottom: number; width: number }

/** Coordinates stay in layout-viewport space, including a panned visual viewport. */
export function popoverPlacement(anchor: AnchorBounds, panel: { width: number; height: number }, viewport: ViewportBounds, matchWidth = false) {
  const edge = 8;
  const gap = 6;
  const maxWidth = Math.max(0, viewport.width - edge * 2);
  const width = Math.min(matchWidth ? anchor.width : panel.width, maxWidth);
  const minTop = viewport.top + edge;
  const maxBottom = viewport.top + viewport.height - edge;
  const below = Math.max(0, maxBottom - anchor.bottom - gap);
  const above = Math.max(0, anchor.top - gap - minTop);
  const upward = panel.height > below && above > below;
  const maxHeight = Math.min(Math.max(0, viewport.height - edge * 2), upward ? above : below);
  const height = Math.min(panel.height, maxHeight);
  return {
    left: Math.max(viewport.left + edge, Math.min(anchor.left, viewport.left + viewport.width - edge - width)),
    top: Math.max(minTop, Math.min(upward ? anchor.top - gap - height : anchor.bottom + gap, maxBottom - height)),
    maxWidth, maxHeight,
    ...(matchWidth ? { width } : {}),
  };
}
