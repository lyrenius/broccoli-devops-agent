#!/usr/bin/env python3
"""Controller-side fixed-endpoint HTTP diagnostics; redirects are not followed."""
import argparse
import http.client
import json
import time


def probe(target):
    routes = {'broccoli-server': '/healthz', 'web-frontend': '/'}
    if target not in routes:
        raise ValueError('unsupported application target')
    start = time.monotonic()
    connection = http.client.HTTPConnection('10.0.19.135', 3000, timeout=5)
    try:
        connection.request('GET', routes[target])
        response = connection.getresponse()
        body = response.read(2048)
        result = {'target': target, 'healthy': response.status == 200, 'http_status': response.status}
        if target == 'broccoli-server':
            result['dependency_health'] = body.decode(errors='replace')
    except OSError as error:
        result = {'target': target, 'healthy': False, 'reason': type(error).__name__}
    finally:
        connection.close()
    result['elapsed_ms'] = round((time.monotonic() - start) * 1000, 1)
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('target', choices=['broccoli-server', 'web-frontend'])
    print(json.dumps(probe(parser.parse_args().target)))
