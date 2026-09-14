import { useLayoutEffect, useRef, useState, type CSSProperties } from 'react';
import { popoverPlacement } from './popoverPlacement';

/** Native non-modal top layer: light dismissal and Escape, without a backdrop or focus trap. */
export function useAnchoredPopover<T extends HTMLElement = HTMLButtonElement & HTMLInputElement>(open: boolean, matchWidth = false) {
  const anchor = useRef<T>(null);
  const panel = useRef<HTMLElement>(null);
  const [position, setPosition] = useState<CSSProperties>({});
  useLayoutEffect(() => {
    const element = panel.current;
    if (!open || !element) return;
    const place = () => {
      const rect = anchor.current?.getBoundingClientRect();
      if (!rect) return;
      const viewport = window.visualViewport;
      const viewportBounds = {
        left: viewport?.offsetLeft ?? 0, top: viewport?.offsetTop ?? 0,
        width: viewport?.width ?? window.innerWidth, height: viewport?.height ?? window.innerHeight,
      };
      // A reopened panel retains the previous viewport's inline maxWidth.
      // Normalize constraints before reading its actual width: measuring the
      // old narrow panel and then widening it places its right edge offscreen.
      const availableWidth = Math.max(0, viewportBounds.width - 16);
      element.style.maxWidth = `${availableWidth}px`;
      if (matchWidth) element.style.width = `${Math.min(rect.width, availableWidth)}px`;
      const bounds = element.getBoundingClientRect();
      const next = popoverPlacement(rect, { width: bounds.width, height: Math.max(bounds.height, element.scrollHeight) }, viewportBounds, matchWidth);
      // ResizeObserver and viewport callbacks must commit coordinates in this
      // layout phase, not expose new dimensions with stale React coordinates.
      element.style.left = `${next.left}px`;
      element.style.top = `${next.top}px`;
      element.style.maxHeight = `${next.maxHeight}px`;
      setPosition((previous) => JSON.stringify(previous) === JSON.stringify(next) ? previous : next);
    };
    element.showPopover();
    place();
    const observer = new ResizeObserver(place);
    observer.observe(element);
    if (anchor.current) observer.observe(anchor.current);
    window.addEventListener('resize', place);
    window.addEventListener('scroll', place, true);
    window.visualViewport?.addEventListener('resize', place);
    window.visualViewport?.addEventListener('scroll', place);
    return () => {
      observer.disconnect();
      window.removeEventListener('resize', place);
      window.removeEventListener('scroll', place, true);
      window.visualViewport?.removeEventListener('resize', place);
      window.visualViewport?.removeEventListener('scroll', place);
      if (element.matches(':popover-open')) element.hidePopover();
    };
  }, [open, matchWidth]);
  return { anchor, panel, position };
}
