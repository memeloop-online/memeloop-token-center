import { useEffect, useState } from 'react';
import { Button } from '../design-system';
import { useI18n } from '../i18n';

const copy = {
  'zh-CN': {
    title: '服务状态', retry: '重新读取状态',
    unknown: '无法读取服务状态。数据是否可用尚未确认；无需因此重新填写服务凭据。',
    initializing: '服务正在启动，暂时无法读取数据或验证权限。',
    database_storage_full: '数据库报告存储空间已满，必要的数据库写入不可用。请联系服务运维；无需重新填写服务凭据。',
    database_unavailable: '数据库暂时不可用，无法确认数据和权限。请等待服务恢复；无需重新填写服务凭据。',
    database_schema_outdated: '数据库结构尚未就绪，暂时无法确认数据可用性。请联系服务运维；无需重新填写服务凭据。',
    initialization: '服务初始化失败，数据和权限验证暂不可用。请联系服务运维；无需重新填写服务凭据。',
    archive: '归档服务暂时不可用，归档内容可能无法读取或保存。',
    history: '最近一次数据库故障', historyFull: '当时数据库报告存储空间已满。', historyUnavailable: '当时数据库连接不可用。', readHealthy: '最近的健康检查通过了读取验证；这不确认写入已经恢复。',
  },
  en: {
    title: 'Service status', retry: 'Read status again',
    unknown: 'Service status cannot be read. Data availability is unverified; you do not need to re-enter service credentials.',
    initializing: 'The service is starting. Data and permission checks are temporarily unavailable.',
    database_storage_full: 'The database reported full storage. Required database writes are unavailable. Contact the service operator; you do not need to re-enter service credentials.',
    database_unavailable: 'The database is unavailable. Data and permissions cannot be verified. Wait for service recovery; you do not need to re-enter service credentials.',
    database_schema_outdated: 'The database schema is not ready. Data availability cannot be verified. Contact the service operator; you do not need to re-enter service credentials.',
    initialization: 'Service initialization failed. Data and permission checks are unavailable. Contact the service operator; you do not need to re-enter service credentials.',
    archive: 'Archive storage is unavailable. Archived content may not be readable or saved.',
    history: 'Last observed database failure', historyFull: 'The database reported full storage at that time.', historyUnavailable: 'The database connection was unavailable at that time.', readHealthy: 'The latest health check verified reads; it does not confirm that writes have recovered.',
  },
} as const;

type Health = {
  status: 'ready' | 'degraded' | 'not_ready';
  error?: { code?: string };
  checks?: { database?: string };
  last_database_error?: { code: string; observed_at: number } | null;
};

export function ServiceAvailabilityFooter() {
  const { locale } = useI18n();
  const text = copy[locale];
  const [health, setHealth] = useState<Health | null>();
  const [refresh, setRefresh] = useState(0);
  useEffect(() => {
    let active = true;
    let request: AbortController | undefined;
    const read = async () => {
      request?.abort();
      const controller = new AbortController();
      request = controller;
      try {
        const response = await fetch('/ui-assets/service-status', {
          cache: 'no-store', credentials: 'omit', signal: controller.signal,
        });
        const value = await response.json() as Health;
        if (![200, 503].includes(response.status) || !['ready', 'degraded', 'not_ready'].includes(value.status)) throw new Error();
        if (active && !controller.signal.aborted) setHealth(value);
      } catch {
        if (active && !controller.signal.aborted) setHealth(null);
      }
    };
    void read();
    const interval = window.setInterval(() => { if (document.visibilityState === 'visible') void read(); }, 30_000);
    return () => { active = false; request?.abort(); window.clearInterval(interval); };
  }, [refresh]);
  if (health === undefined || health?.status === 'ready' && !health.last_database_error) return null;
  const code = health?.error?.code;
  const message = !health ? text.unknown
    : code === 'service_initializing' ? text.initializing
    : code === 'database_storage_full' || code === 'database_unavailable' || code === 'database_schema_outdated' ? text[code]
    : health.status === 'degraded' ? text.archive
    : health.status === 'not_ready' ? text.initialization : undefined;
  const observed = health?.last_database_error;
  const last = observed && ['database_storage_full', 'database_unavailable'].includes(observed.code)
    && Number.isSafeInteger(observed.observed_at) && observed.observed_at >= 0 && observed.observed_at < 8_640_000_000_000
    ? observed : undefined;
  const lastMessage = last?.code === 'database_storage_full' ? text.historyFull : text.historyUnavailable;
  return <footer className="app-service-availability" aria-label={text.title}>
    <div role="status" aria-live="polite">
      {message && <p>{message}</p>}
      {last && <p>{text.history}: <time dateTime={new Date(last.observed_at * 1000).toISOString()}>{new Date(last.observed_at * 1000).toLocaleString(locale)}</time>. {lastMessage} {health?.checks?.database === 'ok' && text.readHealthy}</p>}
    </div>
    <Button appearance="subtle" onClick={() => setRefresh((value) => value + 1)}>{text.retry}</Button>
  </footer>;
}
