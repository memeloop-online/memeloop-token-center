import type { ReactNode } from 'react';
import type { RequestView, StatsBucket } from './types';

export function Shell({ children, operator = false }: { children: ReactNode; operator?: boolean }) {
  return (
    <div className="app-shell">
      <aside className="rail">
        <div className="brand-mark">M</div>
        <div className="rail-line" />
        <div className="rail-label">{operator ? 'OP' : 'SELF'}</div>
      </aside>
      <main className="main">{children}</main>
    </div>
  );
}

export function Metric({ label, value, tone }: { label: string; value: ReactNode; tone?: string }) {
  return (
    <article className={`metric ${tone ?? ''}`}>
      <span>{label}</span>
      <strong>{value}</strong>
    </article>
  );
}

export function Buckets({ values }: { values: StatsBucket[] }) {
  const maximum = Math.max(1, ...values.map((value) => value.requests));
  if (!values.length) return <div className="empty">暂无数据</div>;
  return (
    <div className="bucket-list">
      {values.map((value) => (
        <div className="bucket" key={value.name}>
          <div>
            <b>{value.name}</b>
            <span>{value.requests} 次 · {value.input_tokens + value.output_tokens} tokens</span>
          </div>
          <div className="bar"><i style={{ width: `${(value.requests / maximum) * 100}%` }} /></div>
        </div>
      ))}
    </div>
  );
}

export function RequestTable({
  requests,
  onSelect,
}: {
  requests: RequestView[];
  onSelect?: (request: RequestView) => void;
}) {
  if (!requests.length) return <div className="empty">暂无请求</div>;
  return (
    <div className="table-scroll">
      <table>
        <thead><tr><th>时间</th><th>模型</th><th>协议</th><th>状态</th><th>耗时</th><th>Tokens</th><th>费用</th></tr></thead>
        <tbody>
          {requests.map((request) => (
            <tr key={request.request_id} onClick={() => onSelect?.(request)} className={onSelect ? 'clickable' : ''}>
              <td>{new Date(request.created_at).toLocaleString()}</td>
              <td><code>{request.model}</code></td>
              <td>{request.protocol}</td>
              <td><span className={`status ${request.status_code && request.status_code < 400 ? 'ok' : 'bad'}`}>{request.status_code ?? '运行中'}</span></td>
              <td>{request.duration_ms ?? '—'} ms</td>
              <td>{request.input_tokens + request.output_tokens}</td>
              <td>{request.cost}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
