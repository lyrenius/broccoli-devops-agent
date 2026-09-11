import importlib.util
import io
import json
import pathlib
import types
import unittest
from unittest.mock import patch

ROOT = pathlib.Path(__file__).resolve().parents[1]
def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

infra, app, redis = load('infra-ops'), load('app-health'), load('redis-health')

class OperationalHelpers(unittest.TestCase):
    def test_postgres_uses_fixed_readonly_session_and_no_password_value_in_argv(self):
        with patch.object(infra.subprocess, 'run', return_value=types.SimpleNamespace(stdout='{"connections":4}')) as run:
            self.assertTrue(infra.postgres(infra.CHECK_SQL)['healthy'])
        args = run.call_args.args[0]
        self.assertIn('default_transaction_read_only=on', args[-3])
        self.assertEqual(args[-1], infra.CHECK_SQL)
        self.assertIn('$POSTGRES_PASSWORD', args[-3])
        self.assertTrue(run.call_args.kwargs['capture_output'])

    def test_storage_403_is_gateway_response_not_verified_object_access(self):
        with patch.object(infra, 'http_status', side_effect=[200,403]):
            result = infra.storage()
        self.assertTrue(result['healthy'])
        self.assertFalse(result['object_read_write_verified'])
        with patch.object(infra, 'http_status', side_effect=[200,None]):
            self.assertFalse(infra.storage()['healthy'])

    def test_app_health_returns_dependency_body_and_rejects_unknown_target(self):
        conn = types.SimpleNamespace(request=lambda *args:None,
            getresponse=lambda:types.SimpleNamespace(status=503,read=lambda n:b'db=ok, mq=down'),close=lambda:None)
        with patch.object(app.http.client,'HTTPConnection',return_value=conn):
            result = app.probe('broccoli-server')
        self.assertFalse(result['healthy'])
        self.assertEqual(result['dependency_health'],'db=ok, mq=down')
        with self.assertRaises(ValueError):app.probe('http://untrusted.invalid')

    def test_redis_info_only_exports_allowed_fields(self):
        data=b'redis_version:7.4.11\r\nused_memory:42\r\nrequirepass:do-not-export\r\n'
        class Sock:
            def __enter__(self):return self
            def __exit__(self,*args):pass
            def sendall(self,data):pass
            def makefile(self,*args):return io.BytesIO(b'+OK\r\n+PONG\r\n$'+str(len(data)).encode()+b'\r\n'+data+b'\r\n')
        def read(path,*args,**kwargs):
            return 'infra' if str(path).endswith('broccoli-node-role') else 'REDIS_PASSWORD="dummy-secret"'
        with patch('pathlib.Path.read_text',read),patch.object(redis.socket,'create_connection',return_value=Sock()):
            result=redis.probe(include_info=True)
        self.assertEqual(result['info']['used_memory'],'42')
        self.assertNotIn('do-not-export',json.dumps(result))
        self.assertNotIn('dummy-secret',json.dumps(result))

if __name__ == '__main__':unittest.main()
