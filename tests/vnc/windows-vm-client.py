"""Authorized isolated VM controller. Credential on stdin; NTLM message encryption.

This intentionally has no credential command-line/environment/file option. Input
after the credential consists of explicit JSON requests; request bodies are not
echoed. Only use scripts whose output is safe to retain as test evidence.
"""
import argparse
import json
import sys

from pypsrp.client import Client


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host', required=True)
    parser.add_argument('--user', required=True)
    parser.add_argument('--endpoint', required=True)
    args = parser.parse_args()
    password = sys.stdin.readline().rstrip('\r\n')
    if not password:
        raise ValueError('A credential must arrive on stdin')
    for line in sys.stdin:
        request = json.loads(line)
        result = {'id': request['id'], 'outcome': 'failed'}
        with Client(args.host, username=args.user, password=password, ssl=False,
                auth='ntlm', encryption='always', connection_timeout=10,
                read_timeout=90, operation_timeout=60) as client:
            try:
                if request.get('operation') == 'copy':
                    result['path'] = client.copy(request['source'], request['destination'],
                                                 configuration_name=args.endpoint)
                elif request.get('operation') == 'fetch':
                    client.fetch(request['source'], request['destination'],
                                 configuration_name=args.endpoint)
                else:
                    output, streams, failed = client.execute_ps(request['script'],
                                                                configuration_name=args.endpoint)
                    result.update(output=output, errors=[str(e) for e in streams.error])
                    if failed:
                        raise RuntimeError('Remote pipeline reported an error')
                result['outcome'] = 'passed'
            except Exception as error:
                result['error'] = str(error)
        print(json.dumps(result, ensure_ascii=True), flush=True)
    del password


if __name__ == '__main__':
    main()
