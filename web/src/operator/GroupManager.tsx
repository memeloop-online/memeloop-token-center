import { useConfirmDialog } from '../useConfirmDialog';
import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { GroupKind, GroupView } from '../types';
import { MultiCombobox, type ComboboxOption } from './MultiCombobox';
import { GroupStrategyEditor } from './GroupStrategyEditor';

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
  const loadSequence = useRef(0);
  const scope = useRef({ kind, token, tenant });
  scope.current = { kind, token, tenant };
  const load = async () => {
    const sequence = ++loadSequence.current;
    const loadScope = { kind, token, tenant };
    if (!loadScope.token || !loadScope.tenant) { setGroups([]); return; }
    const query = new URLSearchParams({ tenant_external_id: loadScope.tenant });
    try {
      const next = await api<GroupView[]>(`/internal/v1/${paths[loadScope.kind]}?${query}`, loadScope.token);
      if (sequence !== loadSequence.current || scope.current.kind !== loadScope.kind || scope.current.token !== loadScope.token || scope.current.tenant !== loadScope.tenant) return;
      setGroups(next); setError('');
    }
    catch (reason) {
      if (sequence !== loadSequence.current || scope.current.kind !== loadScope.kind || scope.current.token !== loadScope.token || scope.current.tenant !== loadScope.tenant) return;
      setGroups([]); setError(messageOf(reason, t('groups.loadFailed')));
    }
  };
  useEffect(() => { loadSequence.current += 1; setGroups([]); setError(''); void load(); }, [kind, token, tenant]);
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
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, kind]);
  const [selectedId, setSelectedId] = useState('');
  const [savedGroups, setSavedGroups] = useState<Record<string, GroupView>>({});
  const drafts = useRef({ id: '', membersDirty: false, nameDirty: false });
  const [memberDraft, setMemberDraft] = useState<ComboboxOption[]>([]);
  const [newName, setNewName] = useState('');
  const [renameDraft, setRenameDraft] = useState('');
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const operationSequence = useRef(0);
  const scope = useRef({ kind, token, tenant });
  scope.current = { kind, token, tenant };
  // Mutation responses own the revision until a list read has caught up.
  const visibleGroups = groups.map(group => savedGroups[group.id]?.updated_at > group.updated_at ? savedGroups[group.id] : group)
    .concat(Object.values(savedGroups).filter(saved => !groups.some(group => group.id === saved.id)));
  const rememberSaved = (group: GroupView) => setSavedGroups(current => current[group.id]?.updated_at > group.updated_at ? current : { ...current, [group.id]: group });
  const selected = visibleGroups.find((group) => group.id === selectedId);
  const selectGroup = (id: string) => {
    const group = visibleGroups.find((value) => value.id === id);
    drafts.current = { id, membersDirty: false, nameDirty: false };
    setSelectedId(id);
    setRenameDraft(group?.name ?? '');
    setMemberDraft((group?.member_ids ?? []).map((memberId) => resources.find((item) => item.value === memberId)
      ?? { value: memberId, label: memberId }));
    setMessage(''); setError('');
  };
  const editorGroup = selected ?? visibleGroups[0];
  const groupVersion = editorGroup ? `${editorGroup.id}:${editorGroup.updated_at}` : '';
  const resourceVersion = resources.map((resource) => `${resource.value}:${resource.label}`).join('|');
  useEffect(() => {
    operationSequence.current += 1;
    drafts.current = { id: '', membersDirty: false, nameDirty: false };
    setSavedGroups({}); setSelectedId(''); setMemberDraft([]); setNewName(''); setRenameDraft('');
    setBusy(false); setMessage(''); setError('');
  }, [kind, token, tenant]);
  useEffect(() => {
    const group = selected ?? visibleGroups[0];
    if (!group) {
      if (selectedId) { setSelectedId(''); setRenameDraft(''); setMemberDraft([]); }
      return;
    }
    if (drafts.current.id !== group.id) drafts.current = { id: group.id, membersDirty: false, nameDirty: false };
    setSelectedId(group.id);
    if (!drafts.current.nameDirty) setRenameDraft(group.name);
    if (!drafts.current.membersDirty) setMemberDraft(group.member_ids.map((memberId) => resources.find((item) => item.value === memberId)
      ?? { value: memberId, label: memberId }));
  }, [groupVersion, selectedId]);
  useEffect(() => {
    setMemberDraft(draft => draft.map(member => resources.find(resource => resource.value === member.value) ?? member));
  }, [resourceVersion]);
  useEffect(() => {
    setSavedGroups(current => {
      const caughtUp = Object.keys(current).filter(id => groups.some(group => group.id === id && group.updated_at >= current[id].updated_at));
      if (!caughtUp.length) return current;
      const next = { ...current }; for (const id of caughtUp) delete next[id]; return next;
    });
  }, [groups]);

  const perform = async (action: () => Promise<void | (() => void)>, success: string) => {
    if (busy) return;
    const sequence = ++operationSequence.current;
    const operationScope = { kind, token, tenant };
    setBusy(true); setMessage(''); setError('');
    try {
      const applyResult = await action();
      if (sequence !== operationSequence.current || scope.current.kind !== operationScope.kind || scope.current.token !== operationScope.token || scope.current.tenant !== operationScope.tenant) return;
      applyResult?.();
      setMessage(success); await onChanged();
    }
    catch (reason) {
      if (sequence === operationSequence.current && scope.current.kind === operationScope.kind && scope.current.token === operationScope.token && scope.current.tenant === operationScope.tenant) setError(messageOf(reason, t('common.requestFailed')));
    }
    finally {
      if (sequence === operationSequence.current && scope.current.kind === operationScope.kind && scope.current.token === operationScope.token && scope.current.tenant === operationScope.tenant) setBusy(false);
    }
  };

  return <article className="panel group-manager" data-group-kind={kind}>{confirmationDialog}
    <div className="panel-title"><div><h2>{t(`groups.${kind}.title`)}</h2><p className="muted">{t(`groups.${kind}.description`)}</p></div><span>{formatNumber(groups.length, locale)}</span></div>
    {error && <div className="notice error" role="alert">{error}</div>}
    {message && <div className="notice success" role="status">{message}</div>}
    <form className="group-create" onSubmit={(event) => {
      event.preventDefault();
      const name = newName.trim();
      if (!name) return;
      void perform(async () => {
        const created = await api<GroupView>(`/internal/v1/${paths[kind]}`, token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, name }) });
        return () => { drafts.current = { id: created.id, membersDirty: false, nameDirty: false }; rememberSaved(created); setNewName(''); setSelectedId(created.id); setRenameDraft(created.name); setMemberDraft([]); };
      }, t('groups.created', { name }));
    }}><label>{t('groups.name')}<input maxLength={100} value={newName} onChange={(event) => setNewName(event.target.value)} /></label><button type="submit" disabled={!tenant || busy || !newName.trim()}>{t('groups.create')}</button></form>
    {visibleGroups.length === 0 ? <div className="empty">{t(`groups.${kind}.empty`)}</div> : <div className="group-editor-layout">
      <div className="group-list" role="list" aria-label={t(`groups.${kind}.title`)}>{visibleGroups.map((group) => <button type="button" role="listitem" disabled={busy} className={group.id === selectedId ? 'active' : ''} key={group.id} onClick={() => selectGroup(group.id)}><span>{group.name}</span><small>{t('groups.memberCount', { count: formatNumber(group.member_count, locale) })}</small></button>)}</div>
      {selected && <div className="group-editor">
        <div className="group-rename"><label>{t('groups.name')}<input maxLength={100} disabled={busy} value={renameDraft} onChange={(event) => { drafts.current.nameDirty = true; setRenameDraft(event.target.value); }} /></label><button type="button" className="secondary" disabled={busy || !renameDraft.trim() || renameDraft.trim() === selected.name} onClick={() => void perform(async () => {
          const saved = await api<GroupView>(`/internal/v1/${paths[kind]}/${selected.id}`, token, { method: 'PUT', body: JSON.stringify({ tenant_external_id: tenant, name: renameDraft.trim(), expected_updated_at: selected.updated_at }) });
          return () => { drafts.current.nameDirty = false; rememberSaved(saved); };
        }, t('groups.renamed', { name: renameDraft.trim() }))}>{t('common.save')}</button></div>
        <MultiCombobox disabled={busy} label={t(`groups.${kind}.members`)} options={resources} value={memberDraft} onChange={value => { drafts.current.membersDirty = true; setMemberDraft(value); }} placeholder={t('groups.searchMembers')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} />
        <div className="group-editor-actions"><button type="button" disabled={busy} onClick={() => void perform(async () => {
          const saved = await api<GroupView>(`/internal/v1/${paths[kind]}/${selected.id}/members`, token, { method: 'PUT', body: JSON.stringify({ tenant_external_id: tenant, member_ids: memberDraft.map((item) => item.value), expected_updated_at: selected.updated_at }) });
          return () => { drafts.current.membersDirty = false; rememberSaved(saved); };
        }, t('groups.membersSaved'))}>{t('groups.saveMembers')}</button><button type="button" className="danger" disabled={busy} onClick={async () => {
          if (!await confirm(t('groups.confirmDelete', { name: selected.name }))) return;
          void perform(async () => {
            const query = new URLSearchParams({ tenant_external_id: tenant, expected_updated_at: String(selected.updated_at) });
            await api(`/internal/v1/${paths[kind]}/${selected.id}?${query}`, token, { method: 'DELETE' });
            return () => { setSavedGroups(current => { const next = { ...current }; delete next[selected.id]; return next; }); setSelectedId(''); };
          }, t('groups.deleted', { name: selected.name }));
        }}>{t('common.remove')}</button></div>
        {kind !== 'credential' && <GroupStrategyEditor key={`${token}\0${tenant}\0${kind}\0${selected.id}`} kind={kind} token={token} tenant={tenant} group={selected} onChanged={onChanged} onSaved={rememberSaved} />}
      </div>}
    </div>}
  </article>;
}
