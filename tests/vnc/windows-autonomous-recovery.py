"""Disposable hosted Windows only: real TCP application blackhole, not NIC loss."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe', type=Path, required=True)
    parser.add_argument('--results', type=Path, required=True)
    args = parser.parse_args()
    if os.name != 'nt' or os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted':
        raise RuntimeError('disposable hosted Windows runner required')
    args.results.mkdir(exist_ok=False)
    markers = args.results / 'markers'
    markers.mkdir()
    events, errors, peers = [], [], []
    lock = threading.Lock()
    stop = threading.Event()
    blackhole = threading.Event()
    recovered = threading.Event()
    green = threading.Event()
    started = time.monotonic()

    def emit(**event):
        with lock:
            events.append(dict(elapsed_ms=round((time.monotonic()-started)*1000), **event))

    def exact(conn, length):
        data = bytearray()
        while len(data) < length:
            part = conn.recv(length-len(data))
            if not part:
                raise EOFError()
            data.extend(part)
        return bytes(data)

    def serve(conn, number):
        try:
            conn.settimeout(160)
            conn.sendall(b'RFB 003.008\n')
            assert exact(conn, 12) == b'RFB 003.008\n'
            conn.sendall(b'\x01\x01')
            assert exact(conn, 1) == b'\x01'
            conn.sendall(bytes(4))
            shared = exact(conn, 1)[0]
            pf = struct.pack('>BBBBHHHBBB3x', 32, 24, 0, 1, 255, 255, 255, 0, 8, 16)
            conn.sendall(struct.pack('>HH', 800, 600)+pf+struct.pack('>I', 9)+b'SYNTHETIC')
            emit(event='handshake', connection=number, shared=shared, security='None')
            last_color = None
            while not stop.is_set():
                tag = exact(conn, 1)[0]
                response = b''
                if tag == 0:
                    exact(conn, 3)
                    pf = exact(conn, 16)
                    assert pf == struct.pack('>BBBBHHHBBB3x', 32, 24, 0, 1, 255, 255, 255, 0, 8, 16)
                elif tag == 2:
                    _, count = struct.unpack('>BH', exact(conn, 3))
                    assert count <= 256
                    encodings = struct.unpack(f'>{count}i', exact(conn, count*4))
                    assert -312 in encodings and 16 in encodings
                    emit(event='encodings', connection=number, values=encodings)
                    response = struct.pack('>B3xIB', 248, 0, 0)
                elif tag == 3:
                    exact(conn, 9)
                    rgb = (0, 255, 0) if green.is_set() else ((0, 0, 255) if recovered.is_set() else (255, 0, 0))
                    if rgb != last_color:
                        pixels = bytes((*rgb, 0)) * (800*600)
                        response = struct.pack('>BBHHHHHi', 0, 0, 1, 0, 0, 800, 600, 0)+pixels
                        last_color = rgb
                        emit(event='frame', connection=number, rgb=rgb, sha256=hashlib.sha256(pixels).hexdigest())
                    else:
                        # Static desktop, not a flood of fabricated full frames.
                        stop.wait(.1)
                        response = bytes(4)
                elif tag == 248:
                    flags, length = struct.unpack('>3xIB', exact(conn, 8))
                    assert length <= 64
                    payload = exact(conn, length)
                    emit(event='fence', connection=number, blocked=blackhole.is_set() and number == 1,
                         flags=flags, payload_bytes=length)
                    response = struct.pack('>B3xIB', 248, 0, length)+payload
                else:
                    raise AssertionError(f'unexpected message {tag}; read-only/clipboard policy violated')
                if blackhole.is_set() and number == 1:
                    emit(event='application_blackhole', connection=number, message=tag,
                         withheld_bytes=len(response))
                elif response:
                    conn.sendall(response)
        except (EOFError, ConnectionError, OSError) as error:
            emit(event='socket_closed', connection=number, reason=type(error).__name__)
        except Exception as error:
            errors.append(repr(error))
        finally:
            conn.close()

    listener = socket.socket()
    listener.bind(('127.0.0.1', 0))
    listener.listen(4)
    listener.settimeout(.2)
    port = listener.getsockname()[1]
    threads = []

    def accept():
        while not stop.is_set():
            try:
                conn, remote = listener.accept()
            except TimeoutError:
                continue
            except OSError:
                return
            peers.append(conn)
            emit(event='accept', connection=len(peers), remote=remote)
            thread = threading.Thread(target=serve, args=(conn, len(peers)))
            threads.append(thread)
            thread.start()

    accept_thread = threading.Thread(target=accept)
    accept_thread.start()
    result = dict(outcome='failed', source_sha=os.environ['GITHUB_SHA'], run_id=os.environ['GITHUB_RUN_ID'],
                  scope='Windows production SessionManager, real loopback TCP; synthetic None peer; application response blackhole with TCP still established',
                  limitations=['Not UltraVNC/TigerVNC compatibility or encrypted/authenticated transport proof.',
                               'Application response blackhole is not packet loss, OS TCP keepalive failure, NIC switching or OS sleep.'],
                  probe_sha256=hashlib.sha256(args.probe.read_bytes()).hexdigest(), port=port)
    process = None
    try:
        with (args.results/'client.stdout.txt').open('w', encoding='utf-8') as out, (args.results/'client.stderr.txt').open('w', encoding='utf-8') as err:
            process = subprocess.Popen([str(args.probe.resolve()), '127.0.0.1', str(port), str(markers.resolve()), '--synthetic-none'],
                                       stdin=subprocess.PIPE, stdout=out, stderr=err, text=True)
            process.stdin.write('\n')
            process.stdin.close()

            def await_marker(name, seconds):
                deadline = time.monotonic()+seconds
                while not (markers/name).exists():
                    assert process.poll() is None, f'probe exited {process.returncode} before {name}'
                    assert not errors, errors
                    assert time.monotonic() < deadline, f'deadline waiting for {name}'
                    stop.wait(.05)
                emit(event='marker', name=name)

            await_marker('ready', 20)
            idle_start = time.monotonic()
            # Longer than heartbeat timeout; static pixels must remain connected.
            while time.monotonic()-idle_start < 35:
                assert process.poll() is None and not (markers/'disconnected').exists()
                stop.wait(.1)
            idle_fences = [e for e in events if e['event']=='fence' and not e['blocked']]
            assert len(idle_fences) >= 6, idle_fences
            result['static_idle_seconds'] = time.monotonic()-idle_start
            result['idle_fence_replies'] = len(idle_fences)
            blackhole.set()
            emit(event='blackhole_begin')
            stop.wait(2)
            # Preserve the actual native Windows socket table during the fault.
            snapshot = subprocess.check_output(['pwsh', '-NoProfile', '-Command',
                f'Get-NetTCPConnection -State Established -ErrorAction Stop | Where-Object {{ $_.LocalPort -eq {port} -or $_.RemotePort -eq {port} }} | Select-Object LocalAddress,LocalPort,RemoteAddress,RemotePort,State,OwningProcess | ConvertTo-Json'], text=True)
            (args.results/'fault-sockets.json').write_text(snapshot, encoding='utf-8')
            sockets = json.loads(snapshot)
            assert any(s['OwningProcess']==process.pid for s in sockets)
            await_marker('disconnected', 30)
            assert any(e['event']=='fence' and e['blocked'] for e in events)
            recovered.set()
            await_marker('recovered', 25)
            green.set()
            await_marker('resumed', 15)
            assert process.wait(timeout=5) == 0
            assert not errors, errors
            assert len(peers) == 2, f'expected one reconnect; got {len(peers)} sockets'
            result.update(outcome='passed', connections=len(peers), client_pid=process.pid,
                          assertions=['35s static desktop stayed connected with real Fence replies',
                                      'withheld responses on established Windows TCP caused heartbeat timeout',
                                      'stale red framebuffer withdrawn, same tab and one session',
                                      'fresh blue full frame and read-only/None/encoding policy preserved',
                                      'fresh green full frame after recovery'])
    except Exception as error:
        result['error'] = repr(error)
        raise
    finally:
        if process and process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        stop.set()
        listener.close()
        for conn in peers:
            try:
                conn.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        accept_thread.join(3)
        for thread in threads:
            thread.join(3)
        result['cleanup'] = dict(threads_stopped=not accept_thread.is_alive() and not any(t.is_alive() for t in threads),
                                 client_exited=process is None or process.poll() is not None)
        result['server_errors'] = errors
        (args.results/'events.json').write_text(json.dumps(events, indent=2)+'\n', encoding='utf-8')
        (args.results/'result.json').write_text(json.dumps(result, indent=2)+'\n', encoding='utf-8')
        assert all(result['cleanup'].values()), result['cleanup']


if __name__ == '__main__':
    main()
