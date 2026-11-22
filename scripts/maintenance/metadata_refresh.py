#!/usr/bin/env python3
"""Read cached MTC metadata and render private evidence. Never runs inference.

Live: python3 metadata_refresh.py --state-dir STATE --base-url MANAGEMENT_URL \
  --context CONTEXT --namespace NAMESPACE --secret SECRET
Offline: python3 metadata_refresh.py --state-dir STATE --snapshot SNAPSHOT
Outputs are private; latest.json atomically points to a complete generation.
"""
import argparse
import base64
from collections import defaultdict
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess
import tempfile
import urllib.error
import urllib.parse
import urllib.request

ACCOUNT_FIELDS = ('id', 'name', 'driver', 'status', 'route_count', 'created_at')
ROUTE_FIELDS = ('id', 'public_model', 'upstream_model', 'protocol', 'enabled',
                'upstream_account_id', 'upstream_account_ids', 'candidate_upstream_account_ids')


def read_pages(read, path, fields):
    rows, seen, cursor, pages = [], set(), {}, 0
    while True:
        page = read(path, limit=100, **cursor)
        page = page if isinstance(page, list) else page['items']
        pages += 1
        if not page:
            return rows, pages
        for row in page:
            if row['id'] in seen:
                raise ValueError('pagination repeated an ID; snapshot not published')
            seen.add(row['id'])
            rows.append({key: row.get(key) for key in fields})
        cursor = {'before_created_at': page[-1]['created_at'], 'before_id': page[-1]['id']}



def now():
    return datetime.now(timezone.utc).isoformat()


def load(path):
    return json.loads(Path(path).read_text())


def private_write(path, value):
    path = Path(path)
    fd, temporary = tempfile.mkstemp(prefix='.' + path.name, dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as stream:
            stream.write(value if isinstance(value, str) else json.dumps(value, ensure_ascii=False, indent=2))
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def collect(args):
    base = urllib.parse.urlsplit(args.base_url)
    if base.scheme not in ('http', 'https') or base.username or base.password or base.query or base.fragment:
        raise ValueError('base URL must be a clean management URL')
    if base.scheme == 'http' and base.hostname not in ('127.0.0.1', 'localhost', '::1'):
        raise ValueError('HTTP management access requires loopback forwarding')
    if not base.path.endswith('/internal/v1/'):
        raise ValueError('base URL must end in /internal/v1/')
    secret = json.loads(subprocess.check_output([
        'kubectl', '--context', args.context, '-n', args.namespace,
        'get', 'secret', args.secret, '-o', 'json',
    ], stderr=subprocess.DEVNULL))
    token = base64.b64decode(secret['data'][args.secret_key]).decode()
    del secret
    opener = urllib.request.build_opener(NoRedirect())

    def read(path, **params):
        params['tenant_external_id'] = args.tenant
        request = urllib.request.Request(args.base_url + path + '?' + urllib.parse.urlencode(params),
                                         headers={'Authorization': 'Bearer ' + token})
        with opener.open(request, timeout=15) as response:
            return json.load(response)

    started = now()
    accounts, account_pages = read_pages(read, 'upstreams', ACCOUNT_FIELDS)
    routes, pages = read_pages(read, 'model-routes', ROUTE_FIELDS)
    result = []
    for account in accounts:
        item = {key: account.get(key) for key in ('id', 'name', 'driver', 'status', 'route_count')}
        try:
            catalog = read('upstreams/' + urllib.parse.quote(account['id'], safe='') + '/models', limit=10000)
            item['catalog'] = {key: catalog.get(key) for key in (
                'status', 'credential_generation', 'last_attempt_at', 'last_success_at', 'expires_at', 'error_code',
            )}
            for key in ('models', 'disabled_models'):
                item['catalog'][key] = [{field: model.get(field) for field in ('id', 'protocol')}
                                        for model in catalog.get(key, [])]
            item['catalog_read'] = 'success'
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as error:
            item['catalog_read'] = 'failed'
            item['error_type'] = type(error).__name__
        result.append(item)
    token = None
    return {'schema_version': '0.3', 'collection_started_at': started, 'metadata_updated_at': now(),
            'account_pagination_complete': True, 'account_pages_including_empty_terminal': account_pages,
            'route_pagination_complete': True, 'route_pages_including_empty_terminal': pages,
            'route_count': len(routes), 'accounts': result, 'routes': routes,
            'consistency': 'sequential API observations, not a database transaction snapshot'}


def render(snapshot, canonical, evidence):
    # Preserve canonical directory IDs and exact aliases. Never modify procurement records.
    by_identity = defaultdict(list)
    by_channel_alias = defaultdict(list)
    channel_accounts = defaultdict(set)
    for record in canonical['records']:
        account = record.get('account_id')
        if account:
            channel_accounts[record['channel_id']].add(account)
        aliases = {record.get(key) for key in ('model_id', 'catalog_model_id') if isinstance(record.get(key), str)}
        for alias in aliases:
            by_channel_alias[(record['channel_id'], alias)].append(record['record_id'])
            if account:
                by_identity[(account, alias)].append(record['record_id'])
    account_channels = defaultdict(set)
    for channel, accounts in channel_accounts.items():
        for account in accounts:
            account_channels[account].add(channel)
    # Explicit reviewed bindings only; never infer a supplier from a fuzzy display name.
    for account, channel in evidence.get('channel_bindings', {}).items():
        account_channels[account].add(channel)
    domestic = set(evidence.get('domestic_provider_account_ids', []))
    historical = defaultdict(list)
    for entry in evidence.get('entries', []):
        historical[(entry['account_id'], entry['model_id'], entry['protocol'])].append(entry)
    rows, accounts = [], []
    for account in snapshot['accounts']:
        catalog = account.get('catalog', {})
        channels = sorted(account_channels[account['id']])
        scope = 'domestic_provider' if account['id'] in domestic else 'unknown_provider_scope'
        summary = dict(account)
        summary.pop('catalog', None)
        summary.update(channel_ids=channels, provider_scope=scope,
                       catalog_metadata={k: v for k, v in catalog.items() if k not in ('models', 'disabled_models')},
                       catalog_count=len(catalog.get('models', [])), disabled_count=len(catalog.get('disabled_models', [])),
                       catalog_coverage='unknown' if account.get('catalog_read') != 'success' or any(len(catalog.get(k, [])) >= 10000 for k in ('models', 'disabled_models')) else 'cached_snapshot_complete')
        accounts.append(summary)
        models = {}
        for model in catalog.get('models', []):
            models[(model['id'], model['protocol'])] = {'catalog_present': True}
        for model in catalog.get('disabled_models', []):
            models.setdefault((model['id'], model['protocol']), {})['catalog_disabled'] = True
        for route in snapshot['routes']:
            configured = set(route.get('upstream_account_ids') or []) | {route.get('upstream_account_id')}
            candidates = set(route.get('candidate_upstream_account_ids') or [])
            if account['id'] not in configured | candidates:
                continue
            model = models.setdefault((route['upstream_model'], route['protocol']), {})
            model.setdefault('configured_routes', []).append({key: route.get(key) for key in ('id', 'public_model', 'enabled')})
            model['configured'] = True
            model['explicit_account_binding'] = model.get('explicit_account_binding', False) or account['id'] in configured
            model['candidate_membership'] = model.get('candidate_membership', False) or account['id'] in candidates
        for (alias, protocol), model in sorted(models.items()):
            records = set(by_identity[(account['id'], alias)])
            for channel in channels:
                records.update(by_channel_alias[(channel, alias)])
            history = sorted(historical[(account['id'], alias, protocol)], key=lambda x: x['tested_at'])
            latest = history[-1] if history else None
            rows.append(dict(account_id=account['id'], account_name=account['name'], model_id=alias, protocol=protocol,
                             provider_scope=scope, model_origin='unknown', canonical_record_ids=sorted(records),
                             canonical_mapping='exact_account_or_reviewed_channel_alias' if records else 'unmapped',
                             metadata_updated_at=snapshot['metadata_updated_at'],
                             catalog_present=model.get('catalog_present', False),
                             catalog_supports_protocol=model.get('catalog_present', False) or models.get((alias, 'any'), {}).get('catalog_present', False),
                             catalog_disabled=model.get('catalog_disabled', False), configured=model.get('configured', False),
                             explicit_account_binding=model.get('explicit_account_binding', False), candidate_membership=model.get('candidate_membership', False),
                             configured_routes=model.get('configured_routes', []),
                             verification_status=latest['outcome'] if latest else 'unknown',
                             tested_at=latest['tested_at'] if latest else None, verification_evidence=history))
    return {'schema_version': '0.3', 'rendered_at': now(), 'metadata_updated_at': snapshot['metadata_updated_at'],
            'route_count': snapshot['route_count'], 'route_pagination_complete': snapshot['route_pagination_complete'],
            'account_pagination_complete': snapshot.get('account_pagination_complete', False),
            'account_pages_including_empty_terminal': snapshot.get('account_pages_including_empty_terminal'),
            'accounts': accounts, 'models': rows, 'canonical_directory_records': len(canonical['records']),
            'unmatched_historical_evidence': [entry for key, entries in historical.items() if key not in {(r['account_id'], r['model_id'], r['protocol']) for r in rows} for entry in entries],
            'notes': ['cached catalog presence does not prove callable', 'provider scope does not determine model origin',
                      'configured includes candidates and does not prove caller grants', 'historical evidence is exact identity only',
                      'canonical test_evidence preserved in source, not inferred from incompatible legacy protocol fields']}


def markdown(report):
    lines = ['# 国产上游模型目录与验证状态', '', '元数据更新时间：' + report['metadata_updated_at'],
             '', '完整路由：' + str(report['route_count']) + '；账号/模型/协议条目：' + str(len(report['models'])),
             '', 'catalog 表示缓存记录；configured 表示配置或候选关联；精确实测及其时间单独记录。',
             '国产上游包含聚合转发，模型产地保持未知。映射沿用原采购目录，未匹配条目保留 unmapped。', '',
             '|上游|账号状态|缓存状态|目录条目|已移除|覆盖|', '|---|---|---|---:|---:|---|']
    def cell(value):
        return str(value).replace('|', '\\|').replace('\n', ' ').replace('\r', ' ')
    for a in report['accounts']:
        lines.append('|' + '|'.join(cell(v) for v in [a['name'], a['status'], a['catalog_metadata'].get('status', 'unknown'), a['catalog_count'], a['disabled_count'], a['catalog_coverage']]) + '|')
    lines += ['', '完整精确别名、原采购 record_id、协议、历史证据见同代 report.json。',
              '历史证据与原采购记录保持独立，刷新元数据不会重置 tested_at。',
              '本入口只执行 GET 元数据读取；未执行模型生成、路由修改、部署或业务验收。',
              '模型测试需在指定测试站上线及容量就绪后单独执行。']
    return '\n'.join(lines) + '\n'


def publish(output_dir, snapshot, report):
    generation = Path(tempfile.mkdtemp(prefix='snapshot-', dir=output_dir))
    private_write(generation / 'metadata.json', snapshot)
    private_write(generation / 'report.json', report)
    private_write(generation / 'report.md', markdown(report))
    private_write(output_dir / 'latest.json', {'generation': generation.name, 'metadata_updated_at': report['metadata_updated_at'], 'rendered_at': report['rendered_at']})
    return generation


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--snapshot', type=Path, help='render an existing raw metadata snapshot without network')
    parser.add_argument('--base-url')
    parser.add_argument('--context')
    parser.add_argument('--namespace')
    parser.add_argument('--secret')
    parser.add_argument('--secret-key', default='service-token')
    parser.add_argument('--tenant', default='default')
    parser.add_argument('--state-dir', type=Path, required=True, help='private state directory')
    parser.add_argument('--canonical', type=Path)
    parser.add_argument('--evidence', type=Path)
    parser.add_argument('--output-dir', type=Path)
    args = parser.parse_args()
    if not args.snapshot and not all((args.base_url, args.context, args.namespace, args.secret)):
        parser.error('live refresh requires --base-url --context --namespace --secret')
    args.canonical = args.canonical or args.state_dir / 'canonical.json'
    args.evidence = args.evidence or args.state_dir / 'evidence.json'
    args.output_dir = args.output_dir or args.state_dir / 'metadata-private'
    os.umask(0o077)
    args.output_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    if args.output_dir.stat().st_mode & 0o077:
        raise ValueError('output directory must be private (0700)')
    snapshot = load(args.snapshot) if args.snapshot else collect(args)
    report = render(snapshot, load(args.canonical), load(args.evidence))
    generation = publish(args.output_dir, snapshot, report)
    print(json.dumps({'generation': generation.name, 'metadata_updated_at': report['metadata_updated_at'], 'route_count': report['route_count'], 'model_entries': len(report['models'])}))


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        # Do not emit response bodies, requests, headers, credentials or subprocess output.
        print(json.dumps({'status': 'failed', 'error_type': type(error).__name__}))
        raise SystemExit(1)
