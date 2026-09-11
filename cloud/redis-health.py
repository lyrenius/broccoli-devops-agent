#!/usr/bin/env python3
"""Root-owned, fixed-target Redis diagnostic. Emits no credentials or raw errors."""
import json
import pathlib
import shlex
import socket
import time
import argparse


def probe(include_info=False):
    if pathlib.Path('/etc/broccoli-node-role').read_text().strip() != 'infra':
        raise RuntimeError('This diagnostic is restricted to the infra node')
    password = None
    for line in pathlib.Path('/opt/broccoli/.env').read_text().splitlines():
        name, sep, value = line.partition('=')
        if sep and name.strip() == 'REDIS_PASSWORD':
            parts = shlex.split(value, comments=True)
            if len(parts) == 1:
                password = parts[0]
    if not password:
        raise RuntimeError('Redis credential is not configured')
    started = time.monotonic()
    result = {'target': 'redis-mq', 'tcp_reachable': False, 'responsive': False}
    stage = 'tcp_connect'
    try:
        with socket.create_connection(('10.0.22.32', 6379), timeout=2) as sock:
            result['tcp_reachable'] = True
            with sock.makefile('rb') as stream:
                for name, args, expected in [('auth', ('AUTH', password), b'+OK\r\n'),
                                             ('ping', ('PING',), b'+PONG\r\n')]:
                    stage = name
                    parts = [arg.encode() for arg in args]
                    sock.sendall(b'*' + str(len(parts)).encode() + b'\r\n' + b''.join(
                        b'$' + str(len(p)).encode() + b'\r\n' + p + b'\r\n' for p in parts))
                    if stream.readline(4096) != expected:
                        result.update(failed_stage=stage, reason='unexpected_response')
                        break
                else:
                    result['responsive'] = True
                    result['reply'] = 'PONG'
                    if include_info:
                        stage = 'info'
                        sock.sendall(b'*1\r\n$4\r\nINFO\r\n')
                        header = stream.readline(64)
                        if not header.startswith(b'$'):
                            raise OSError('unexpected info response')
                        size = int(header[1:-2])
                        if not 0 <= size <= 262144:
                            raise OSError('oversized info response')
                        payload = stream.read(size + 2)
                        if len(payload) != size + 2 or not payload.endswith(b'\r\n'):
                            raise OSError('incomplete info response')
                        allowed = {'redis_version', 'uptime_in_seconds', 'connected_clients',
                                   'blocked_clients', 'used_memory', 'used_memory_peak', 'maxmemory',
                                   'maxmemory_policy', 'evicted_keys', 'aof_enabled',
                                   'aof_last_write_status', 'aof_last_bgrewrite_status',
                                   'loading', 'role', 'total_commands_processed'}
                        result['info'] = {key: value for line in payload[:-2].decode().splitlines()
                                          if ':' in line for key, value in [line.split(':', 1)]
                                          if key in allowed}
    except (OSError, TimeoutError) as error:
        result.update(failed_stage=stage,
                      reason='timeout' if isinstance(error, TimeoutError) else 'connection_error')
    result['elapsed_ms'] = round((time.monotonic() - started) * 1000, 1)
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--info', action='store_true')
    print(json.dumps(probe(parser.parse_args().info)), flush=True)
