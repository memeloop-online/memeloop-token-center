import { useEffect, useRef, useState } from 'react';
import { ApiError, api } from '../api';
import { Button, Textarea } from '../design-system';
import { useI18n } from '../i18n';
import { SafeMarkdown } from '../safeMarkdown';
import type { UpstreamAccount, UpstreamAccountNotes } from '../types';
import { messageOf } from './scope/operatorShared';
import { FormSection } from './FormSection';
import { upstreamNotesDirty, upstreamNotesUpdateBody, validateUpstreamNotes } from './upstreamNotes';
import { upstreamNotesCopy } from './upstreamNotesCopy';
import './upstreamNotes.css';

interface UpstreamNotesProps {
  account: UpstreamAccount;
  token: string;
  tenant: string;
  disabled: boolean;
  onDirtyChange: (dirty: boolean) => void;
  onSaved: (notes: string | null, updatedAt: number) => void;
}

/** Markdown notes for one upstream account; saves through the dedicated notes endpoint. */
export function UpstreamNotes({ account, token, tenant, disabled, onDirtyChange, onSaved }: UpstreamNotesProps) {
  const { locale, t } = useI18n();
  const copy = upstreamNotesCopy(locale);
  const baseline = useRef<string | null>(account.notes ?? null);
  const [draft, setDraft] = useState(account.notes ?? '');
  const [preview, setPreview] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [savedNotice, setSavedNotice] = useState(false);
  const textarea = useRef<HTMLTextAreaElement>(null);
  const accountId = account.id;

  useEffect(() => {
    baseline.current = account.notes ?? null;
    setDraft(account.notes ?? '');
    setError('');
    setSavedNotice(false);
    setPreview(false);
  }, [accountId]);

  useEffect(() => {
    const next = account.notes ?? null;
    if (next !== baseline.current) {
      baseline.current = next;
      setDraft(next ?? '');
    }
  }, [account.notes]);

  const issue = validateUpstreamNotes(draft);
  const dirty = upstreamNotesDirty(baseline.current, draft);

  useEffect(() => {
    onDirtyChange(dirty);
    return () => onDirtyChange(false);
  }, [dirty, onDirtyChange]);

  function changeDraft(next: string, selection?: [number, number]) {
    setDraft(next);
    setSavedNotice(false);
    if (selection) requestAnimationFrame(() => {
      const area = textarea.current;
      if (area) { area.focus(); area.setSelectionRange(selection[0], selection[1]); }
    });
  }

  function surround(prefix: string, suffix: string, placeholder: string) {
    const area = textarea.current;
    const start = area?.selectionStart ?? draft.length;
    const end = area?.selectionEnd ?? draft.length;
    const selected = draft.slice(start, end) || placeholder;
    const next = `${draft.slice(0, start)}${prefix}${selected}${suffix}${draft.slice(end)}`;
    changeDraft(next, [start + prefix.length, start + prefix.length + selected.length]);
  }

  function insertLink() {
    const area = textarea.current;
    const start = area?.selectionStart ?? draft.length;
    const end = area?.selectionEnd ?? draft.length;
    const selected = draft.slice(start, end) || copy.linkPlaceholder;
    const next = `${draft.slice(0, start)}[${selected}](https://)${draft.slice(end)}`;
    changeDraft(next, [start + selected.length + 3, start + selected.length + 11]);
  }

  function bulletList() {
    const area = textarea.current;
    const start = area?.selectionStart ?? draft.length;
    const end = area?.selectionEnd ?? draft.length;
    const selected = draft.slice(start, end) || copy.listPlaceholder;
    const lines = selected.split('\n').map((line) => line.trim() ? `- ${line}` : line).join('\n');
    const next = `${draft.slice(0, start)}${lines}${draft.slice(end)}`;
    changeDraft(next, [start, start + lines.length]);
  }

  async function save() {
    if (saving || disabled || !dirty || issue) return;
    setSaving(true);
    setError('');
    setSavedNotice(false);
    try {
      const result = await api<UpstreamAccountNotes>(`/internal/v1/upstreams/${account.id}/notes`, token, {
        method: 'PATCH',
        body: JSON.stringify(upstreamNotesUpdateBody(draft, tenant, account.updated_at)),
      });
      if (result.id !== account.id || typeof result.updated_at !== 'number') throw new Error(copy.saveUnconfirmed);
      const savedNotes = result.notes ?? null;
      baseline.current = savedNotes;
      setDraft(savedNotes ?? '');
      setSavedNotice(true);
      onSaved(savedNotes, result.updated_at);
    } catch (reason) {
      setError(reason instanceof ApiError && reason.status === 409 ? copy.stale : messageOf(reason, t('common.requestFailed')));
    } finally {
      setSaving(false);
    }
  }

  function cancel() {
    setDraft(baseline.current ?? '');
    setError('');
    setSavedNotice(false);
  }

  return <FormSection title={copy.title} description={copy.description}>
    <div className="upstream-notes-editor">
      <div className="segmented" role="group" aria-label={copy.modeLabel}>
        <Button appearance="secondary" type="button" size="small" aria-pressed={!preview} className={preview ? '' : 'active'} disabled={saving} onClick={() => setPreview(false)}>{copy.edit}</Button>
        <Button appearance="secondary" type="button" size="small" aria-pressed={preview} className={preview ? 'active' : ''} disabled={saving} onClick={() => setPreview(true)}>{copy.preview}</Button>
      </div>
      {preview ? (draft.trim()
        ? <SafeMarkdown source={draft} className="upstream-notes-preview" />
        : <p className="muted">{copy.empty}</p>) : <>
        <div className="row-actions upstream-notes-toolbar" role="toolbar" aria-label={copy.toolbarLabel}>
          <Button appearance="secondary" type="button" size="small" disabled={disabled || saving} onClick={() => surround('**', '**', copy.boldPlaceholder)}>{copy.bold}</Button>
          <Button appearance="secondary" type="button" size="small" disabled={disabled || saving} onClick={() => surround('*', '*', copy.italicPlaceholder)}>{copy.italic}</Button>
          <Button appearance="secondary" type="button" size="small" disabled={disabled || saving} onClick={insertLink}>{copy.link}</Button>
          <Button appearance="secondary" type="button" size="small" disabled={disabled || saving} onClick={bulletList}>{copy.bulletList}</Button>
        </div>
        <Textarea ref={textarea} value={draft} rows={7} resize="vertical" disabled={disabled || saving} placeholder={copy.placeholder} aria-label={copy.title} onChange={(_, data) => changeDraft(data.value)} />
        <small className="field-hint">{copy.formatHint}</small>
      </>}
      {issue && <div className="notice error" role="alert">{issue === 'tooLong' ? copy.tooLong : copy.controlChars}</div>}
      {error && <div className="notice error" role="alert">{error}</div>}
      {savedNotice && !dirty && <div className="notice success" role="status">{copy.saved}</div>}
      <div className="row-actions">
        <Button appearance="primary" type="button" disabled={disabled || saving || !dirty || Boolean(issue)} onClick={() => void save()}>{saving ? copy.saving : copy.save}</Button>
        <Button appearance="secondary" type="button" disabled={saving || !dirty} onClick={cancel}>{copy.cancel}</Button>
      </div>
    </div>
  </FormSection>;
}
