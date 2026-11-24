"""Synthetic contracts; run only through CI."""
import copy
import importlib.util
import json
import pathlib
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('metadata_refresh', pathlib.Path(__file__).with_name('metadata_refresh.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def snapshot():
    return {'metadata_updated_at': '2030-01-02T00:00:00Z', 'route_count': 0,
            'route_pagination_complete': True, 'account_pagination_complete': True,
            'account_pages_including_empty_terminal': 2, 'routes': [],
            'accounts': [{'id': 'synthetic-a', 'name': 'Synthetic provider', 'status': 'active',
                          'catalog_read': 'success', 'catalog': {'status': 'ready',
                          'models': [{'id': 'Exact-Alias', 'protocol': 'any'}], 'disabled_models': []}}]}


class MetadataContracts(unittest.TestCase):
    def test_all_pages_and_allowlist(self):
        for endpoint, fields in [('upstreams', m.ACCOUNT_FIELDS), ('model-routes', m.ROUTE_FIELDS)]:
            calls = []
            def read(path, **params):
                calls.append((path, params))
                index = len(calls)
                if index == 3:
                    return []
                return [{'id': 'item-' + str(index), 'created_at': 30 - index,
                         'credential': 'SYNTHETIC_SECRET', 'proxy_password': 'SYNTHETIC_SECRET'}]
            rows, pages = m.read_pages(read, endpoint, fields)
            self.assertEqual((len(rows), pages), (2, 3))
            self.assertEqual(calls[1][1]['before_id'], 'item-1')
            self.assertEqual(calls[2][1]['before_created_at'], 28)
            self.assertNotIn('SYNTHETIC_SECRET', json.dumps(rows))

    def test_duplicate_cursor_rejected(self):
        def read(path, **params):
            return [{'id': 'duplicate', 'created_at': 1}]
        with self.assertRaises(ValueError):
            m.read_pages(read, 'upstreams', m.ACCOUNT_FIELDS)

    def test_history_survives_refresh_scope_is_explicit(self):
        data = snapshot()
        canonical = {'records': [{'record_id': 'record-a', 'account_id': 'synthetic-a',
                                  'channel_id': 'channel-a', 'model_id': 'Exact-Alias'}]}
        evidence = {'entries': [{'account_id': 'synthetic-a', 'model_id': 'Exact-Alias',
                                'protocol': 'any', 'tested_at': '2029-12-01T00:00:00Z',
                                'outcome': 'tested_success'}]}
        before = copy.deepcopy(evidence)
        first = m.render(data, canonical, evidence)
        data['metadata_updated_at'] = '2030-02-01T00:00:00Z'
        second = m.render(data, canonical, evidence)
        self.assertEqual(first['models'][0]['tested_at'], second['models'][0]['tested_at'])
        self.assertEqual(evidence, before)
        self.assertEqual(second['models'][0]['provider_scope'], 'unknown_provider_scope')
        self.assertEqual(second['models'][0]['canonical_record_ids'], ['record-a'])
        self.assertTrue(second['account_pagination_complete'])

    def test_failed_generation_keeps_atomic_latest(self):
        data = snapshot()
        report = m.render(data, {'records': []}, {'entries': []})
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            first = m.publish(root, data, report)
            original = (root / 'latest.json').read_bytes()
            self.assertTrue((first / 'report.md').is_file())
            real_write = m.private_write
            def fail_report(path, value):
                if pathlib.Path(path).name == 'report.json':
                    raise OSError('synthetic interruption')
                return real_write(path, value)
            with patch.object(m, 'private_write', fail_report):
                with self.assertRaises(OSError):
                    m.publish(root, data, report)
            self.assertEqual((root / 'latest.json').read_bytes(), original)
            self.assertEqual((first / 'report.json').stat().st_mode & 0o777, 0o600)


if __name__ == '__main__':
    unittest.main()
