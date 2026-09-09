import { useLayoutEffect, useRef, useState, type CSSProperties } from 'react';

/** Native non-modal top layer: light dismissal and Escape, without a backdrop or focus trap. */
export function useAnchoredPopover(open: boolean) {
  const anchor = useRef<HTMLButtonElement & HTMLInputElement>(null);
  const panel = useRef<HTMLElement>(null);
  const [position, setPosition] = useState<CSSProperties>({});
  useLayoutEffect(() => {
    const element = panel.current;
    if (!open || !element) return;
    element.showPopover();
    const place = () => {
      const rect = anchor.current?.getBoundingClientRect();
      if (!rect) return;
      const width = element.getBoundingClientRect().width;
      const top = Math.min(rect.bottom + 6, Math.max(8, window.innerHeight - 240));
      setPosition({
        left: Math.max(8, Math.min(rect.left, window.innerWidth - width - 8)),
        top,
        maxHeight: Math.max(120, window.innerHeight - top - 8),
      });
    };
    place();
    window.addEventListener('resize', place);
    window.addEventListener('scroll', place, true);
    return () => {
      window.removeEventListener('resize', place);
      window.removeEventListener('scroll', place, true);
      if (element.matches(':popover-open')) element.hidePopover();
    };
  }, [open]);
  return { anchor, panel, position };
}
