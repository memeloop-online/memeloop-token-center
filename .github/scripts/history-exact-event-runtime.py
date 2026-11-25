"""Run-owned synthetic OCI CLI contract; never connects to Stage or production."""
import json
import os
from pathlib import Path
import subprocess
import time
import uuid

IMAGE = 'ghcr.io/memeloop-online/memeloop-token-center:maintenance-1073-ff088e3e56ac702efae5765e68f1e452279646db-45609689934@sha256:ebc2def3623266990328ecf972c61998951fbbfcb00e17969cfdd5a377dcbe75'
SOURCE = 'ff088e3e56ac702efae5765e68f1e452279646db'
IDENTITY = 'https://github.com/memeloop-online/memeloop-token-center/.github/workflows/exact-reader-contract.yml@refs/heads/ci/history-event-only-proof-20261125'
run = 'history1073_' + os.environ['GITHUB_RUN_ID'] + '_' + os.environ['GITHUB_RUN_ATTEMPT']
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

def cli(*args):
    argv = ['docker', 'run', '--rm', '--network', network]
    for key, value in env.items():
        argv += ['-e', f'{key}={value}']
    result = command(*argv, IMAGE, *args)
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
    bootstrap = cli('request-history', 'bootstrap')
    assert sql('SELECT mode FROM request_history_state WHERE id=1') == 'off'
    cli('request-history', 'prepare-activation')
    cli('request-history', 'activate')
    assert cli('request-history', 'capture-horizon')['capture_horizon_ready'] is True
    assert sql('SELECT mode FROM request_history_state WHERE id=1') == 'shadow'
    tenant, key, target, neighbour = [str(uuid.uuid4()) for _ in range(4)]
    created = 42
    for event in [target, neighbour]:
        parent = str(uuid.uuid4())
        sql(f"INSERT INTO request_events(event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,status_code,input_tokens,output_tokens,cost_micros) VALUES('{event}','{tenant}','{key}','{parent}',42,'finished','openai','synthetic-runtime',200,1,2,0)")
    revision = int(sql(f"SELECT revision FROM request_history_export WHERE source='event' AND request_id='{target}'"))
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
    (evidence / 'receipt.json').write_text(json.dumps({'image': IMAGE, 'source': SOURCE, 'database': run, 'table_uuid': table_uuid, 'identity':row, 'wrong_scope':wrong_report,'exact_ack':report,'repeat':repeated,'cursor_unchanged':True,'neighbour_untouched':True}, indent=2))
    print('isolated immutable CLI exact event: PASS')
finally:
    for container in [pg, ch]:
        command('docker', 'rm', '-f', container, check=False)
    command('docker', 'network', 'rm', network, check=False)
