"""Run-owned synthetic OCI CLI contract; never connects to Stage or production."""
import json
import os
from pathlib import Path
import subprocess
import time
import uuid

IMAGE = 'ghcr.io/memeloop-online/memeloop-token-center:maintenance-1073-ff088e3e56ac702efae5765e68f1e452279646db-45609689934@sha256:ebc2def3623266990328ecf972c61998951fbbfcb00e17969cfdd5a377dcbe75'
SOURCE = 'ff088e3e56ac702efae5765e68f1e452279646db'
CHECKPOINT = os.environ.get('HISTORY_RUNTIME_PROFILE') == 'checkpoint1086'
if CHECKPOINT:
    SOURCE = '3e9b5fb00f343899b24f13a7e2826ce73a64250d'
    artifact = Path(os.environ['HISTORY_BUILD_EVIDENCE'])
    assert (artifact / 'source.txt').read_text().strip() == SOURCE
    IMAGE = (artifact / 'immutable-image.txt').read_text().strip()
    assert IMAGE.startswith('ghcr.io/memeloop-online/memeloop-token-center:maintenance-1086-' + SOURCE + '-45679706853@sha256:')
    assert IMAGE.rsplit('sha256:', 1)[1] == '561cd3f535c833db4db1456da5deeb1cedbf4ce71052e77217a580032f0d1717'
IDENTITY = 'https://github.com/memeloop-online/memeloop-token-center/.github/workflows/exact-reader-contract.yml@refs/heads/ci/history-event-only-proof-20261125'
run = ('checkpoint1086_' if CHECKPOINT else 'history1073_') + os.environ['GITHUB_RUN_ID'] + '_' + os.environ['GITHUB_RUN_ATTEMPT']
pg, ch = run + '_pg', run + '_ch'
network = run + '_net'
evidence = Path('runtime-evidence')
evidence.mkdir(exist_ok=True)

def command(*args, check=True):
    result = subprocess.run(args, text=True, capture_output=True, timeout=180)
    if check and result.returncode:
        raise RuntimeError(f'{args[0]} failed: {result.stderr[-4000:]} {result.stdout[-2000:]}')
    return result

def sql(query):
    return command('docker', 'exec', pg, 'psql', '-X', '-U', 'fixture', '-d', run,
                   '-At', '-v', 'ON_ERROR_STOP=1', '-c', query).stdout.strip()

def click(query):
    return command('docker', 'exec', ch, 'clickhouse-client', '--database', run,
                   '--query', query).stdout.strip()

env = {
    'MTC_DATABASE_URL': f'postgres://fixture:fixture@{pg}:5432/{run}',
    'MTC_DATABASE_MAX_CONNECTIONS': '2',
    'MTC_KEY_PEPPER': 'synthetic-fixture-pepper-' + 'x' * 40,
    'MTC_SERVICE_TOKEN': 'synthetic-fixture-token-' + 'y' * 40,
    'MTC_REQUEST_HISTORY_URL': f'http://{ch}:8123',
    'MTC_REQUEST_HISTORY_DATABASE': run,
    'MTC_REQUEST_HISTORY_USER': 'default',
    'MTC_S3_BUCKET': 'synthetic-unused',
    'MTC_S3_ENDPOINT': f'http://{ch}:9009',
    'MTC_S3_ALLOW_HTTP': 'true',
    'MTC_S3_ACCESS_KEY': 'synthetic', 'MTC_S3_SECRET_KEY': 'synthetic',
}

def cli(*args, expect_changed=False):
    argv = ['docker', 'run', '--rm', '--network', network]
    for key, value in env.items():
        argv += ['-e', f'{key}={value}']
    result = command(*argv, IMAGE, *args, check=not expect_changed)
    if expect_changed:
        assert result.returncode != 0 and 'changed' in (result.stdout + result.stderr).lower()
        return {'changed': True}
    if args[0] == 'migrate':
        return None
    return json.loads(result.stdout.strip().splitlines()[-1])

def fingerprint():
    return {
        'queue': sql("SELECT COALESCE(json_agg(t ORDER BY source,request_id)::text,'[]') FROM request_history_export t"),
        'immutable': sql("SELECT COALESCE(json_agg(t ORDER BY revision)::text,'[]') FROM request_history_immutable_outbox t"),
        'identities': sql("SELECT COALESCE(json_agg(t ORDER BY source,request_id)::text,'[]') FROM request_history_identities t"),
        'cursor': sql("SELECT COALESCE(json_agg(t ORDER BY source)::text,'[]') FROM request_history_backfill t"),
        'ch': click('SELECT count() FROM request_history_v1 FINAL'),
    }

try:
    command('docker', 'pull', IMAGE)
    revision = command('docker', 'inspect', '--format', '{{ index .Config.Labels "org.opencontainers.image.revision" }}', IMAGE).stdout.strip()
    assert revision == SOURCE
    signature = command(os.environ['TRUSTED_COSIGN'], 'verify', '--certificate-oidc-issuer', 'https://token.actions.githubusercontent.com',
                        '--certificate-identity', IDENTITY, IMAGE).stdout
    verified = json.loads(signature)
    assert isinstance(verified, list) and verified
    assert all(item['critical']['image']['docker-manifest-digest'] == IMAGE.split('@')[1] for item in verified)
    (evidence / 'signature.json').write_text(signature)
    command('docker', 'network', 'create', network)
    command('docker', 'run', '-d', '--name', pg, '--network', network,
            '-e', 'POSTGRES_USER=fixture', '-e', 'POSTGRES_PASSWORD=fixture',
            '-e', f'POSTGRES_DB={run}', 'postgres:16-alpine')
    command('docker', 'run', '-d', '--name', ch, '--network', network,
            '-e', 'CLICKHOUSE_SKIP_USER_SETUP=1', 'clickhouse/clickhouse-server:25.4.2')
    for _ in range(60):
        if command('docker', 'exec', pg, 'pg_isready', '-U', 'fixture', check=False).returncode == 0 and command('docker', 'exec', ch, 'clickhouse-client', '--query', 'SELECT 1', check=False).returncode == 0:
            break
        time.sleep(1)
    else:
        raise RuntimeError('isolated services not ready')
    command('docker', 'exec', ch, 'clickhouse-client', '--query', f'CREATE DATABASE {run}')
    cli('migrate')
    def seed_events():
        tenant, key, first, second = [str(uuid.uuid4()) for _ in range(4)]
        target, neighbour = sorted([first, second])
        for event in [target, neighbour]:
            parent = str(uuid.uuid4())
            sql(f"INSERT INTO request_events(event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,status_code,input_tokens,output_tokens,cost_micros) VALUES('{event}','{tenant}','{key}','{parent}',42,'finished','openai','synthetic-runtime',200,1,2,0)")
        return tenant, key, target, neighbour
    pages = []
    if CHECKPOINT:
        # Historical synthetic rows exist before source capture is installed.
        tenant, key, target, neighbour = seed_events()
    bootstrap = cli('request-history', 'bootstrap')
    assert sql('SELECT mode FROM request_history_state WHERE id=1') == 'off'
    cli('request-history', 'prepare-activation')
    cli('request-history', 'activate')
    assert cli('request-history', 'capture-horizon')['capture_horizon_ready'] is True
    assert sql('SELECT mode FROM request_history_state WHERE id=1') == 'shadow'
    if CHECKPOINT:
        for source in ['archive', 'generation']:
            empty = cli('request-history', 'backfill-page', '--source', source, '--page-size', '1')
            assert empty['queued'] == 0
            assert sql(f"SELECT done FROM request_history_backfill WHERE source='{source}'") == '1'
    if not CHECKPOINT:
        tenant, key, target, neighbour = seed_events()
    created = 42
    if CHECKPOINT:
        def cursor():
            return json.loads(sql("SELECT json_build_object('source',source,'after_id',after_id,'after_created_at',after_created_at,'legacy_after_id',legacy_after_id,'epoch',(SELECT epoch FROM request_history_activation WHERE id=1),'done',done<>0) FROM request_history_backfill WHERE source='event'"))
        initial = cursor()
        def page(expected, expect_changed=False):
            return cli('request-history', 'backfill-page', '--source', 'event', '--page-size', '1', '--expected-checkpoint', json.dumps(expected), expect_changed=expect_changed)
        before = fingerprint()
        wrong_epoch = dict(initial, epoch=initial['epoch'] + 1)
        page(wrong_epoch, expect_changed=True)
        assert fingerprint() == before
        first = page(initial)
        assert first['before'] == initial and first['visited'] == first['actually_enqueued'] == 1
        assert first['covered'] == first['conflict'] == 0 and first['identities'][0]['request_id'] == target
        pages.append(first)
        after_first = fingerprint()
        page(initial, expect_changed=True)
        assert fingerprint() == after_first
        second = page(first['after'])
        assert second['before'] == first['after'] and second['visited'] == second['actually_enqueued'] == 1
        assert second['covered'] == second['conflict'] == 0 and second['identities'][0]['request_id'] == neighbour
        assert first['after']['after_created_at'] == second['after']['after_created_at'] == 42
        pages.append(second)
        end = page(second['after'])
        assert end['visited'] == 0 and end['after']['done'] is True
        stable = fingerprint()
        noop = page(end['after'])
        assert noop['before'] == noop['after'] == end['after'] and noop['visited'] == noop['actually_enqueued'] == noop['covered'] == noop['conflict'] == 0
        assert fingerprint() == stable
        for source in ['archive','generation','native']:
            assert [x for x in json.loads(stable['cursor']) if x['source']==source] == [x for x in json.loads(before['cursor']) if x['source']==source]
        pages += [end, noop]
    revision = int(sql(f"SELECT revision FROM request_history_export WHERE source='event' AND request_id='{target}'"))
    if CHECKPOINT:
        assert pages[0]['identities'][0]['revision'] == revision
        neighbour_revision = int(sql(f"SELECT revision FROM request_history_export WHERE source='event' AND request_id='{neighbour}'"))
        assert pages[1]['identities'][0]['revision'] == neighbour_revision
    scope = ['request-history', 'export-exact-event', '--request-id', target,
             '--tenant-id', tenant, '--key-id', key, '--created-at', str(created), '--revision', str(revision)]
    def neighbour_state():
        return {table: sql(f"SELECT COALESCE(json_agg(t ORDER BY request_id)::text,'[]') FROM {table} t WHERE request_id='{neighbour}'") for table in ['request_history_export', 'request_history_immutable_outbox', 'request_history_identities']}
    neighbour_before = neighbour_state()
    before = fingerprint()
    wrong = scope.copy()
    wrong[wrong.index('--key-id') + 1] = str(uuid.uuid4())
    wrong_report = cli(*wrong)
    assert wrong_report['status'] == 'not_ready' and wrong_report['proof'] == 'unproven' and wrong_report['operation_failed'] is True
    assert wrong_report['attempted'] == wrong_report['acked'] == 0
    assert fingerprint() == before
    report = cli(*scope)
    assert report['status'] == 'completed' and report['proof'] == 'exact_ack' and report['attempted'] == report['acked'] == 1
    receipt = sql(f"SELECT acknowledged_revision||':'||acknowledged_hash FROM request_history_identities WHERE source='event' AND request_id='{target}' AND tenant_id='{tenant}' AND key_id='{key}' AND created_at=42")
    acknowledged_revision, acknowledged_hash = receipt.split(':')
    assert int(acknowledged_revision) == revision and len(acknowledged_hash) == 64
    row = json.loads(click(f"SELECT source,request_id,tenant_id,key_id,created_at,revision,snapshot_hash FROM request_history_v1 FINAL WHERE request_id=toUUID('{target}') FORMAT JSONEachRow"))
    assert (row['source'],row['request_id'],row['tenant_id'],row['key_id'],int(row['created_at']),int(row['revision']),row['snapshot_hash']) == ('event',target,tenant,key,created,revision,acknowledged_hash)
    assert sql(f"SELECT count(*) FROM request_history_export WHERE request_id='{target}'") == '0'
    assert sql(f"SELECT count(*) FROM request_history_immutable_outbox WHERE request_id='{target}'") == '0'
    assert sql(f"SELECT count(*) FROM request_history_export WHERE request_id='{neighbour}' AND snapshot IS NULL") == '1'
    assert click(f"SELECT count() FROM request_history_v1 FINAL WHERE request_id=toUUID('{neighbour}')") == '0'
    assert neighbour_state() == neighbour_before
    after = fingerprint()
    assert after['cursor'] == before['cursor']
    repeated = cli(*scope)
    assert repeated['status'] == 'not_ready' and repeated['proof'] == 'unproven' and repeated['operation_failed'] is True
    assert repeated['attempted'] == repeated['acked'] == 0
    assert fingerprint() == after
    table_uuid = click(f"SELECT toString(uuid) FROM system.tables WHERE database='{run}' AND name='request_history_v1'")
    assert table_uuid and uuid.UUID(table_uuid).int != 0
    (evidence / 'receipt.json').write_text(json.dumps({'image': IMAGE, 'source': SOURCE, 'database': run, 'table_uuid': table_uuid, 'identity':row, 'wrong_scope':wrong_report,'exact_ack':report,'repeat':repeated,'cursor_unchanged_during_export':True,'neighbour_untouched':True,'checkpoint_pages':pages,'other_source_cursors_unchanged_during_event_pages':CHECKPOINT,'done_event_noop_unchanged':CHECKPOINT,'oci_page_scope':'event' if CHECKPOINT else None}, indent=2))
    print('isolated immutable CLI checkpoint and exact event: PASS' if CHECKPOINT else 'isolated immutable CLI exact event: PASS')
finally:
    for container in [pg, ch]:
        command('docker', 'rm', '-f', container, check=False)
    command('docker', 'network', 'rm', network, check=False)
