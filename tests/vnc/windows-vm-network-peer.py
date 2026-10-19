"""Synthetic VncAuth/ZRLE peer for an authorized private Windows VM.

Public fixture credential: adit-ci. The fixed challenge/response deliberately
provides no security; a source-scoped VM firewall must protect this test port.
Unlike the application-blackhole fixture this server never advertises Fence.
"""
import argparse
import ipaddress
import json
from pathlib import Path
import socket
import struct
import threading
import time
import zlib

PF = struct.pack('>BBBBHHHBBB3x', 32, 24, 0, 1, 255, 255, 255, 0, 8, 16)
RESPONSE = bytes.fromhex('b179f88c8c74ae9eb179f88c8c74ae9e')


def exact(conn, count):
    value = bytearray()
    while len(value) < count:
        chunk = conn.recv(count-len(value))
        if not chunk:
            raise EOFError()
        value.extend(chunk)
    return bytes(value)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bind', required=True)
    parser.add_argument('--port', type=int, required=True)
    parser.add_argument('--directory', type=Path, required=True)
    args = parser.parse_args()
    address = ipaddress.IPv4Address(args.bind)
    if not (address.is_private or address.is_loopback):
        raise ValueError('Only an authorized private test interface is accepted')
    args.directory.mkdir(exist_ok=False)
    stopping = threading.Event()
    lock = threading.Lock()
    threads = []
    peers = []
    started = time.monotonic()
    with (args.directory/'events.jsonl').open('x', encoding='utf-8') as output:
        def emit(**event):
            with lock:
                output.write(json.dumps(dict(elapsed_ms=round((time.monotonic()-started)*1000), **event))+'\n')
                output.flush()

        def serve(conn, number):
            try:
                conn.settimeout(180)
                conn.sendall(b'RFB 003.008\n')
                assert exact(conn, 12) == b'RFB 003.008\n'
                conn.sendall(b'\x01\x02')
                assert exact(conn, 1) == b'\x02'
                conn.sendall(bytes(16))
                assert exact(conn, 16) == RESPONSE
                conn.sendall(bytes(4))
                shared = exact(conn, 1)[0]
                name = b'ADIT SYNTHETIC NO FENCE'
                conn.sendall(struct.pack('>HH', 800, 600)+PF+struct.pack('>I', len(name))+name)
                emit(event='authenticated', connection=number, security='VncAuth', shared=shared, fence=False)
                compressor = zlib.compressobj()
                previous = None
                while not stopping.is_set():
                    tag = exact(conn, 1)[0]
                    if tag == 0:
                        exact(conn, 3)
                        assert exact(conn, 16) == PF
                    elif tag == 2:
                        _, count = struct.unpack('>BH', exact(conn, 3))
                        assert count <= 256
                        encodings = struct.unpack(f'>{count}i', exact(conn, count*4))
                        assert 16 in encodings
                        emit(event='encodings', connection=number, values=encodings)
                    elif tag == 3:
                        exact(conn, 9)
                        color_path = args.directory/'color.txt'
                        color = color_path.read_text().strip() if color_path.exists() else 'red'
                        rgb = {'red': (255, 0, 0), 'blue': (0, 0, 255), 'green': (0, 255, 0)}[color]
                        if color != previous:
                            tiles = (b'\x01'+bytes(rgb))*(13*10)
                            encoded = compressor.compress(tiles)+compressor.flush(zlib.Z_SYNC_FLUSH)
                            conn.sendall(struct.pack('>BBHHHHHiI', 0, 0, 1, 0, 0, 800, 600, 16, len(encoded))+encoded)
                            emit(event='frame', connection=number, rgb=rgb)
                            previous = color
                        else:
                            stopping.wait(.1)
                            conn.sendall(bytes(4))
                    else:
                        raise AssertionError(f'Unexpected message {tag}; Fence/read-only policy violated')
            except (EOFError, ConnectionError, OSError) as error:
                emit(event='closed', connection=number, reason=type(error).__name__)
            except Exception as error:
                emit(event='error', connection=number, error=repr(error))
            finally:
                conn.close()

        with socket.socket() as listener:
            listener.bind((args.bind, args.port))
            listener.listen(4)
            listener.settimeout(.2)
            (args.directory/'ready.json').write_text(json.dumps(dict(port=args.port, fence=False)))
            try:
                while time.monotonic()-started < 1800 and not (args.directory/'stop.request').exists():
                    try:
                        conn, remote = listener.accept()
                    except TimeoutError:
                        continue
                    peers.append(conn)
                    emit(event='accepted', connection=len(peers))
                    worker = threading.Thread(target=serve, args=(conn, len(peers)))
                    threads.append(worker)
                    worker.start()
            finally:
                stopping.set()
                for conn in peers:
                    try:
                        conn.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                for worker in threads:
                    worker.join(3)
                assert all(not worker.is_alive() for worker in threads)
                emit(event='cleanup', workers_stopped=True, accepted=len(peers))


if __name__ == '__main__':
    main()
