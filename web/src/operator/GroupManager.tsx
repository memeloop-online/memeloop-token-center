import { useEffect, useState } from 'react';
import { api } from '../api';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { GroupKind, GroupView } from '../types';
import { MultiCombobox, type ComboboxOption } from './MultiCombobox';

const paths: Record<GroupKind, string> = {
  provider: 'provider-groups',
  route: 'route-groups',
  credential: 'credential-groups',
};

function messageOf(reason: unknown, fallback: string) {
  return reason instanceof Error ? reason.message : fallback;
}

export function useGroups(kind: GroupKind, token: string, tenant: string) {
  const { t } = useI18n();
  const [groups, setGroups] = useState<GroupView[]>([]);
  const [error, setError] = useState('');
  const load = async () => {
    if (!token || !tenant) { setGroups([]); return; }
    const query = new URLSearchParams({ tenant_external_id: tenant });
    try { setGroups(await api<GroupView[]>(`/internal/v1/${paths[kind]}?${query}`, token)); setError(''); }
    catch (reason) { setGroups([]); setError(messageOf(reason, t('groups.loadFailed'))); }
  };
  useEffect(() => { void load(); }, [kind, token, tenant]);
  return { groups, error, load };
}

interface GroupManagerProps {
  kind: GroupKind;
  token: string;
  tenant: string;
  groups: GroupView[];
  resources: ComboboxOption[];
  onChanged: () => Promise<void>;
}

export function GroupManager({ kind, token, tenant, groups, resources, onChanged }: GroupManagerProps) {
  const { locale, t } = useI18n();
  const [selectedId, setSelectedId] = useState('');
  const [memberDraft, setMemberDraft] = useState<ComboboxOption[]>([]);
  const [newName, setNewName] = useState('');
  const [renameDraft, setRenameDraft] = useState('');
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const selected = groups.find((group) => group.id === selectedId);
  const selectGroup = (id: string) => {
    const group = groups.find((value) => value.id === id);
    setSelectedId(id);
    setRenameDraft(group?.name ?? '');
    setMemberDraft((group?.member_ids ?? []).map((memberId) => resources.find((item) => item.value === memberId)
      ?? { value: memberId, label: memberId }));
    setMessage(''); setError('');
  };
  const groupVersion = groups.map((group) => `${group.id}:${group.updated_at}`).join('|');
  const resourceVersion = resources.map((resource) => `${resource.value}:${resource.label}`).join('|');
  useEffect(() => {
    const group = groups.find((value) => value.id === selectedId) ?? groups[0];
    if (!group) {
      if (selectedId) { setSelectedId(''); setRenameDraft(''); setMemberDraft([]); }
      return;
    }
    setSelectedId(group.id);
    setRenameDraft(group.name);
    setMemberDraft(group.member_ids.map((memberId) => resources.find((item) => item.value === memberId)
      ?? { value: memberId, label: memberId }));
  }, [groupVersion, resourceVersion, selectedId]);

  const perform = async (action: () => Promise<void>, success: string) => {
    setBusy(true); setMessage(''); setError('');
    try { await action(); setMessage(success); await onChanged(); }
    catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(false); }
  };

  return <article className="panel group-manager" data-group-kind={kind}>
    <div className="panel-title"><div><h2>{t(`groups.${kind}.title`)}</h2><p className="muted">{t(`groups.${kind}.description`)}</p></div><span>{formatNumber(groups.length, locale)}</span></div>
    {error && <div className="notice error" role="alert">{error}</div>}
    {message && <div className="notice success" role="status">{message}</div>}
    <form className="group-create" onSubmit={(event) => {
      event.preventDefault();
      const name = newName.trim();
      if (!name) return;
      void perform(async () => {
        const created = await api<GroupView>(`/internal/v1/${paths[kind]}`, token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, name }) });
        setNewName(''); setSelectedId(created.id); setRenameDraft(created.name); setMemberDraft([]);
      }, t('groups.created', { name }));
    }}><label>{t('groups.name')}<input maxLength={100} value={newName} onChange={(event) => setNewName(event.target.value)} /></label><button type="submit" disabled={!tenant || busy || !newName.trim()}>{t('groups.create')}</button></form>
    {groups.length === 0 ? <div className="empty">{t(`groups.${kind}.empty`)}</div> : <div className="group-editor-layout">
      <div className="group-list" role="list" aria-label={t(`groups.${kind}.title`)}>{groups.map((group) => <button type="button" role="listitem" className={group.id === selectedId ? 'active' : ''} key={group.id} onClick={() => selectGroup(group.id)}><span>{group.name}</span><small>{t('groups.memberCount', { count: formatNumber(group.member_count, locale) })}</small></button>)}</div>
      {selected && <div className="group-editor">
        <div className="group-rename"><label>{t('groups.name')}<input maxLength={100} value={renameDraft} onChange={(event) => setRenameDraft(event.target.value)} /></label><button type="button" className="secondary" disabled={busy || !renameDraft.trim() || renameDraft.trim() === selected.name} onClick={() => void perform(async () => {
          await api(`/internal/v1/${paths[kind]}/${selected.id}`, token, { method: 'PUT', body: JSON.stringify({ tenant_external_id: tenant, name: renameDraft.trim(), expected_updated_at: selected.updated_at }) });
        }, t('groups.renamed', { name: renameDraft.trim() }))}>{t('common.save')}</button></div>
        <MultiCombobox label={t(`groups.${kind}.members`)} options={resources} value={memberDraft} onChange={setMemberDraft} placeholder={t('groups.searchMembers')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} />
        <div className="group-editor-actions"><button type="button" disabled={busy} onClick={() => void perform(async () => {
          await api(`/internal/v1/${paths[kind]}/${selected.id}/members`, token, { method: 'PUT', body: JSON.stringify({ tenant_external_id: tenant, member_ids: memberDraft.map((item) => item.value), expected_updated_at: selected.updated_at }) });
        }, t('groups.membersSaved'))}>{t('groups.saveMembers')}</button><button type="button" className="danger" disabled={busy} onClick={() => {
          if (!window.confirm(t('groups.confirmDelete', { name: selected.name }))) return;
          void perform(async () => {
            const query = new URLSearchParams({ tenant_external_id: tenant, expected_updated_at: String(selected.updated_at) });
            await api(`/internal/v1/${paths[kind]}/${selected.id}?${query}`, token, { method: 'DELETE' });
            setSelectedId('');
          }, t('groups.deleted', { name: selected.name }));
        }}>{t('common.remove')}</button></div>
      </div>}
    </div>}
  </article>;
}
