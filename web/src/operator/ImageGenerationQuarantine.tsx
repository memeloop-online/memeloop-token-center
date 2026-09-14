import { useEffect, useRef, useState } from 'react';
import { ApiError, api } from '../api';
import { useI18n } from '../i18n';
import { useConfirmDialog } from '../useConfirmDialog';
import { exactMicros } from './quarantineAmount';

type Resolution = { resolution_id: string; request_id: string; tenant_external_id: string; action: string; confirmed_cost_micros: number; currency: string; evidence_digest: string; resolved_by_service_id: string; resulting_status: string; created_at: number };
type Item = { request_id: string; tenant_external_id: string; model: string; currency: string; reserved_micros: number; submission_started_at: number; submission_uncertain_at: number; revision: string; status: 'awaiting_confirmation' | 'resolved'; resolution: Resolution | null };
type Props = { token: string; tenant: string; writeTenant: string };
function decimalMicros(value: number) {
  if (!Number.isSafeInteger(value) || value < 0) return undefined;
  const integer = BigInt(value);
  return `${integer / 1_000_000n}.${String(integer % 1_000_000n).padStart(6, '0')}`;
}
export function ImageGenerationQuarantine(props: Props) {
  // Remount before displaying a different authority: no old rows or pending drafts flash.
  return <QuarantineEntry key={JSON.stringify([props.token, props.tenant, props.writeTenant])} {...props} />;
}
function QuarantineEntry(props: Props) {
  const { t } = useI18n();
  const [opened, setOpened] = useState(false);
  // Ordinary generation access (including global operators) does not imply the
  // tenant-bound quarantine capability. Only an explicit action starts its reads.
  if (opened) return <ScopedQuarantine {...props} />;
  return <article className="panel image-generation-quarantine"><h2>{t('quarantine.title')}</h2><p className="muted">{t('quarantine.description')}</p>
    {!props.tenant && <p className="notice">{t('quarantine.tenantRequired')}</p>}
    <button type="button" className="secondary" disabled={!props.tenant || !props.token.trim()} onClick={() => setOpened(true)}>{t('quarantine.open')}</button>
  </article>;
}
function ScopedQuarantine({ token, tenant, writeTenant }: Props) {
  const { t, locale } = useI18n();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, writeTenant]);
  const [items, setItems] = useState<Item[]>([]);
  const [detail, setDetail] = useState<Item>();
  const [action, setAction] = useState('not_delivered');
  const [amount, setAmount] = useState('0');
  const [evidence, setEvidence] = useState('');
  const [verified, setVerified] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const [more, setMore] = useState(false);
  const alive = useRef(true);
  const sequence = useRef(0);
  const pending = useRef<{ body: string; key: string } | undefined>(undefined);
  const locked = useRef(false);
  const query = '?tenant_external_id=' + encodeURIComponent(tenant);
  const path = '/internal/v1/image-generation-quarantine';
  const writable = Boolean(token.trim() && tenant && writeTenant === tenant);
  const load = async (after?: string) => {
    if (!tenant || !token.trim()) return;
    const request = ++sequence.current;
    setBusy(true); setError('');
    try {
      const rows = await api<Item[]>(path + query + '&limit=100' + (after ? '&after_id=' + encodeURIComponent(after) : ''), token);
      if (!alive.current || request !== sequence.current) return;
      setItems(current => after ? [...current, ...rows.filter(row => row.tenant_external_id === tenant)] : rows.filter(row => row.tenant_external_id === tenant));
      setMore(rows.length === 100);
    } catch (reason) { if (alive.current && request === sequence.current) setError(reason instanceof Error ? reason.message : t('quarantine.failed')); }
    finally { if (alive.current && request === sequence.current) setBusy(false); }
  };
  useEffect(() => { alive.current = true; void load(); return () => { alive.current = false; sequence.current++; }; }, []);
  const select = async (item: Item) => {
    if (locked.current) return;
    const request = ++sequence.current;
    setBusy(true); setError('');
    try {
      const next = await api<Item>(path + '/' + encodeURIComponent(item.request_id) + query, token);
      if (!alive.current || request !== sequence.current || next.tenant_external_id !== tenant) return;
      setDetail(next); setAction('not_delivered'); setAmount('0'); setEvidence(''); setVerified(false); pending.current = undefined; setMessage('');
    } catch (reason) { if (alive.current && request === sequence.current) setError(reason instanceof Error ? reason.message : t('quarantine.failed')); }
    finally { if (alive.current && request === sequence.current) setBusy(false); }
  };
  const micros = exactMicros(action === 'not_delivered' ? '0' : amount);
  const valid = writable && detail?.tenant_external_id === tenant && detail.status === 'awaiting_confirmation' && /^[a-f0-9]{64}$/.test(detail.revision) && Number.isSafeInteger(detail.reserved_micros) && micros !== undefined && /^[a-f0-9]{64}$/.test(evidence) && verified;
  const resolve = async () => {
    if (!valid || !detail || locked.current) return;
    locked.current = true; setBusy(true); setError(''); setMessage('');
    try {
      if (!await confirm(t('quarantine.confirm', { tenant, action: t('quarantine.' + action), amount: decimalMicros(micros!)!, currency: detail.currency })) || !alive.current) return;
      const body = JSON.stringify({ tenant_external_id: tenant, expected_revision: detail.revision, action, confirmed_cost_micros: micros, currency: detail.currency, evidence_digest: evidence });
      if (pending.current?.body !== body) pending.current = { body, key: crypto.randomUUID() };
      const receipt = await api<Resolution>(path + '/' + encodeURIComponent(detail.request_id) + '/resolve', token, { method: 'POST', body, headers: { 'Idempotency-Key': pending.current.key } });
      if (!alive.current) return;
      pending.current = undefined; setDetail({ ...detail, status: 'resolved', resolution: receipt }); setEvidence(''); setAmount('0'); setVerified(false);
      setItems(rows => rows.filter(row => row.request_id !== detail.request_id)); setMessage(t('quarantine.saved'));
    } catch (reason) {
      if (!alive.current) return;
      if (reason instanceof ApiError && reason.status === 409) {
        setVerified(false);
        setError(t('quarantine.conflict'));
        try {
          const next = await api<Item>(path + '/' + encodeURIComponent(detail.request_id) + query, token);
          if (alive.current && next.tenant_external_id === tenant) setDetail(next);
        } catch { if (alive.current) { setDetail(current => current ? { ...current, revision: '' } : current); setError(t('quarantine.refreshFailed')); } }
      } else setError(reason instanceof Error ? reason.message : t('quarantine.failed'));
    } finally { locked.current = false; if (alive.current) setBusy(false); }
  };
  const time = (value: number) => new Date(value).toLocaleString(locale === 'en' ? 'en-US' : 'zh-CN');
  return <article className="panel image-generation-quarantine">
    {confirmationDialog}<div className="panel-title"><div><h2>{t('quarantine.title')}</h2><p className="muted">{t('quarantine.description')}</p></div><button type="button" className="secondary" disabled={busy || !tenant || !token.trim()} onClick={() => void load()}>{t('usage.refresh')}</button></div>
    {!tenant && <p className="notice">{t('quarantine.tenantRequired')}</p>}
    {tenant && !writable && <p className="notice">{t('quarantine.readOnly')}</p>}
    {error && <p role="alert" className="notice error">{error}</p>}{message && <p role="status" className="notice success">{message}</p>}
    {tenant && items.length === 0 && <p>{busy ? t('common.loading') : t('quarantine.empty')}</p>}
    {items.length > 0 && <div className="table-scroll"><table><thead><tr><th>{t('request.id')}</th><th>{t('request.model')}</th><th>{t('quarantine.reserved')}</th><th>{t('quarantine.uncertainAt')}</th><th>{t('request.actions')}</th></tr></thead><tbody>{items.map(item => <tr key={item.request_id}><td className="break-anywhere">{item.request_id}</td><td>{item.model}</td><td>{decimalMicros(item.reserved_micros) ?? t('quarantine.unsafeAmount')} {item.currency}</td><td>{time(item.submission_uncertain_at)}</td><td><button type="button" disabled={busy} onClick={() => void select(item)}>{t('generations.details')}</button></td></tr>)}</tbody></table></div>}
    {more && <button type="button" disabled={busy} onClick={() => void load(items.at(-1)?.request_id)}>{t('quarantine.more')}</button>}
    {detail && <section aria-label={t('quarantine.details')}><h3>{t('quarantine.details')}</h3><p className="break-anywhere">{detail.request_id} · {tenant} · {detail.model}</p><p>{t('quarantine.reason')}</p><p>{t('quarantine.startedAt')}: {time(detail.submission_started_at)} · {t('quarantine.uncertainAt')}: {time(detail.submission_uncertain_at)}</p><p>{t('quarantine.reserved')}: {decimalMicros(detail.reserved_micros) ?? t('quarantine.unsafeAmount')} {detail.currency}</p>
      {detail.resolution && <section aria-label={t('quarantine.receipt')}><h4>{t('quarantine.receipt')}</h4><p>{t('quarantine.' + detail.resolution.action)} · {decimalMicros(detail.resolution.confirmed_cost_micros) ?? t('quarantine.unsafeAmount')} {detail.resolution.currency}</p><p className="break-anywhere">{t('quarantine.actor')}: {detail.resolution.resolved_by_service_id}</p><p>{time(detail.resolution.created_at)}</p><p className="break-anywhere">{t('quarantine.receiptId')}: {detail.resolution.resolution_id}</p><p className="break-anywhere">{t('quarantine.evidence')}: {detail.resolution.evidence_digest}</p><p>{t('request.status')}: {detail.resolution.resulting_status}</p></section>}
      <fieldset disabled={busy || !writable || detail.status !== 'awaiting_confirmation'}><label>{t('quarantine.action')}<select value={action} onChange={event => { setAction(event.target.value); setVerified(false); }}><option value="not_delivered">{t('quarantine.not_delivered')}</option><option value="settle_confirmed">{t('quarantine.settle_confirmed')}</option></select></label>
      <label>{t('quarantine.amount')} ({detail.currency})<input inputMode="decimal" maxLength={32} value={action === 'not_delivered' ? '0' : amount} readOnly={action === 'not_delivered'} onChange={event => { setAmount(event.target.value); setVerified(false); }} /></label>
      {micros === undefined && <p role="alert">{t('quarantine.invalidAmount')}</p>}
      <label>{t('quarantine.evidence')}<input value={evidence} maxLength={64} pattern="[a-f0-9]{64}" onChange={event => { setEvidence(event.target.value); setVerified(false); }} /></label><p className="muted">{t('quarantine.evidenceHint')}</p>
      <label><input type="checkbox" checked={verified} onChange={event => setVerified(event.target.checked)} />{t('quarantine.verified')}</label>
      <button type="button" className="danger" disabled={!valid || !detail.revision} onClick={() => void resolve()}>{t('quarantine.resolve')}</button></fieldset>
    </section>}
  </article>;
}
