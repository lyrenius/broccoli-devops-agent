#!/usr/bin/env python3
"""Fixed read-only infra diagnostics; no user-supplied SQL, paths or URLs."""
import argparse
import http.client
import json
import os
import pathlib
import shutil
import subprocess
import time

COMPOSE = ['docker', 'compose', '--env-file', '/opt/broccoli/.env', '-f', '/opt/broccoli/compose.yaml']
CHECK_SQL = """SELECT json_build_object(
 'database',current_database(), 'server_version',current_setting('server_version'),
 'max_connections',current_setting('max_connections')::int,
 'connections',(SELECT count(*) FROM pg_stat_activity),
 'active',(SELECT count(*) FROM pg_stat_activity WHERE state='active'),
 'idle_in_transaction',(SELECT count(*) FROM pg_stat_activity WHERE state='idle in transaction'),
 'in_recovery',pg_is_in_recovery());"""
LOCKS_SQL = """SELECT coalesce(json_agg(x), '[]'::json) FROM (
 SELECT pid, pg_blocking_pids(pid) AS blocking_pids, wait_event_type, wait_event,
 round(extract(epoch FROM (clock_timestamp()-xact_start))) AS transaction_age_seconds
 FROM pg_stat_activity WHERE cardinality(pg_blocking_pids(pid)) > 0
 ORDER BY xact_start NULLS LAST LIMIT 20) x;"""


def postgres(sql):
    shell = ('export PGPASSWORD="$POSTGRES_PASSWORD" '
             'PGOPTIONS="-c statement_timeout=3000 -c default_transaction_read_only=on"; '
             'exec psql -X -t -A -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -c "$1"')
    try:
        result = subprocess.run(COMPOSE + ['exec', '-T', 'db', 'sh', '-c', shell, 'sh', sql],
                                capture_output=True, text=True, timeout=8, check=True)
        return {'healthy': True, 'result': json.loads(result.stdout)}
    except (subprocess.SubprocessError, ValueError, OSError) as error:
        return {'healthy': False, 'reason': type(error).__name__}


def http_status(port):
    connection = http.client.HTTPConnection('10.0.22.32', port, timeout=3)
    try:
        connection.request('GET', '/')
        response = connection.getresponse()
        response.read(1024)
        return response.status
    except OSError:
        return None
    finally:
        connection.close()


def storage():
    master, s3 = http_status(9333), http_status(8333)
    return {'healthy': master == 200 and s3 in (200, 403), 'master_http_status': master,
            's3_http_status': s3, 'object_read_write_verified': False}


def resources():
    memory = {}
    for line in pathlib.Path('/proc/meminfo').read_text().splitlines():
        key, value = line.split(':', 1)
        if key in ('MemTotal', 'MemAvailable', 'SwapTotal', 'SwapFree'):
            memory[key + '_bytes'] = int(value.split()[0]) * 1024
    disk = shutil.disk_usage('/var/lib/docker')
    return {'cpu_count': os.cpu_count(), 'load_average': os.getloadavg(), 'memory': memory,
            'docker_disk_bytes': {'total': disk.total, 'used': disk.used, 'free': disk.free}}


def ready(target):
    if target == 'postgres-main':
        return postgres(CHECK_SQL)['healthy']
    if target == 'seaweedfs-storage':
        return storage()['healthy']
    if target == 'redis-mq':
        result = subprocess.run(['/usr/bin/python3', '/usr/local/lib/broccoli/redis-health.py'],
                                capture_output=True, text=True, timeout=8, check=True)
        return json.loads(result.stdout)['responsive']
    raise ValueError('unsupported readiness target')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['postgres-check', 'postgres-locks', 'storage-check', 'resources', 'wait'])
    parser.add_argument('target', choices=['postgres-main', 'redis-mq', 'seaweedfs-storage'])
    args = parser.parse_args()
    if pathlib.Path('/etc/broccoli-node-role').read_text().strip() != 'infra':
        parser.error('infra role required')
    if args.operation.startswith('postgres-') and args.target != 'postgres-main':
        parser.error('PostgreSQL diagnostic requires postgres-main')
    if args.operation == 'storage-check' and args.target != 'seaweedfs-storage':
        parser.error('storage diagnostic requires seaweedfs-storage')
    if args.operation == 'wait':
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            if ready(args.target):
                print(json.dumps({'target': args.target, 'ready': True}))
                return
            time.sleep(1)
        raise SystemExit('service readiness timeout')
    if args.operation == 'postgres-check':
        result = postgres(CHECK_SQL)
    elif args.operation == 'postgres-locks':
        result = postgres(LOCKS_SQL)
    elif args.operation == 'storage-check':
        result = storage()
    else:
        result = resources()
    print(json.dumps({'target': args.target, **result}))


if __name__ == '__main__':
    main()
