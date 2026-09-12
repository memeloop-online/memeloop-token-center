import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { MultiCombobox, type ComboboxOption } from '../../src/operator/MultiCombobox';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const options = Array.from({ length: 20 }, (_, index) => ({ value: String(index), label: `Workspace ${index + 1}`, description: 'A reusable selection field with a descriptive secondary line' }));
function Fixture() {
  const [selected, setSelected] = useState<ComboboxOption[]>([]);
  const [query, setQuery] = useState('');
  const [error, setError] = useState(false);
  return <main style={{ padding: 24, maxWidth: 600, margin: '0 auto' }}>
    <h1>Resource selection</h1>
    <button onClick={() => setError(true)}>Simulate unavailable search</button>
    <div style={{ overflow: 'hidden', height: 160, padding: 8, marginTop: 40 }}>
      <MultiCombobox label="Workspaces" options={options} value={selected} onChange={setSelected}
        placeholder="Search workspaces" emptyText="No matching workspaces" removeLabel={label => `Remove ${label}`}
        hint="Choose one or more workspaces. Existing permissions are unchanged until you save."
        onQueryChange={setQuery} error={error ? 'Search unavailable. Try again.' : ''} retryLabel="Retry search" onRetry={() => setError(false)} />
    </div>
    <button>Continue</button>
    <output aria-label="Selected count">{selected.length}</output>
    <output aria-label="Search query">{query || 'empty'}</output>
  </main>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
