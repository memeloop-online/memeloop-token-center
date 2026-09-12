import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { useAnchoredPopover } from '../../src/useAnchoredPopover';

function Fixture() {
  const [open, setOpen] = useState(false);
  const { anchor, panel, position } = useAnchoredPopover(open);
  return <>
    <button ref={anchor} id="trigger" style={{ position: 'fixed', right: 8, top: 8 }} onClick={() => setOpen(!open)}>Toggle</button>
    {open && <section ref={panel} id="panel" popover="manual" style={{
      boxSizing: 'border-box', width: 'calc(100vw - 16px)', height: 160,
      inset: 'auto', margin: 0, position: 'fixed', ...position,
    }}>Synthetic width regression</section>}
  </>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
