"""Read only named native result files through a temporary read-only NBD export.

No VM stop, snapshot, image copy, filesystem mount or filesystem write. An active
filesystem is not a point-in-time snapshot; results describe flushed bytes only.
"""
import argparse
import datetime
import hashlib
import io
import json
import os
from pathlib import Path
import re
import runpy
import socket
import struct
import uuid
import zlib

KIT = Path('/Users/adalundhe/g121-a4-winarm')
EXPORT = 'step10-flushed-results'
SOCKET = KIT / 'readonly-log-nbd.sock'
FILES = (
    'shards-step10/full-real-04/results.jsonl',
    'shards-step10/full-real04-normal-controller.txt',
    'shards-step10/full-real04-normal-child.exit',
    'shards-step10/full-real04-normal-start.json',
)
CONTAINERS = (
    'shards-step10/full-real-04/081-containers.stdout.txt',
    'shards-step10/full-real-04/081-containers.stderr.txt',
)
DIAGNOSTICS = (
    'Windows/System32/winevt/Logs/System.evtx',
    'Windows/System32/winevt/Logs/OpenSSH%4Operational.evtx',
    'ProgramData/ssh/sshd_config',
    'ProgramData/ssh/logs/sshd.log',
)


class Monitor:
    def __init__(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(20)
        self.sock.connect(str(KIT / 'monitor.sock'))
        self.transcript = self.prompt()

    def prompt(self):
        result = b''
        while not result.endswith(b'(qemu) '):
            part = self.sock.recv(65536)
            if not part:
                raise EOFError('Monitor closed')
            result += part
        return result

    def command(self, command):
        self.sock.sendall((command + '\n').encode())
        result = self.prompt()
        self.transcript += result
        return result


class ReadOnlyNbd:
    """Only NBD READ and DISC exist here. Refuse exports without READ_ONLY."""
    def __init__(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(30)
        self.sock.connect(str(SOCKET))
        assert self.exact(16) == b'NBDMAGICIHAVEOPT'
        global_flags, = struct.unpack('!H', self.exact(2))
        assert global_flags & 1, 'Fixed-newstyle handshake required'
        client_flags = 1 | (global_flags & 2)
        self.sock.sendall(struct.pack('!I', client_flags))
        name = EXPORT.encode()
        self.sock.sendall(struct.pack('!QII', 0x49484156454f5054, 1, len(name)) + name)
        self.size, self.flags = struct.unpack('!QH', self.exact(10))
        if not client_flags & 2:
            assert self.exact(124) == bytes(124)
        assert self.flags & 2, 'Export is not read-only; do not read it'
        self.handle = 0
        self.bytes_read = 0
        self.requests = 0

    def exact(self, size):
        output = bytearray()
        while len(output) < size:
            part = self.sock.recv(size - len(output))
            if not part:
                raise EOFError('NBD stream closed')
            output += part
        return bytes(output)

    def read_at(self, offset, size):
        if size < 0 or size > 1048576 or offset < 0 or offset + size > self.size:
            raise ValueError('Unbounded or out-of-range raw disk read refused')
        if not size:
            return b''
        start = offset & ~511
        end = (offset + size + 511) & ~511
        output = bytearray()
        for position in range(start, end, 1048576):
            length = min(1048576, end - position)
            self.handle += 1
            self.sock.sendall(struct.pack('!IHHQQI', 0x25609513, 0, 0,
                                          self.handle, position, length))
            magic, error, handle = struct.unpack('!IIQ', self.exact(16))
            assert magic == 0x67446698 and handle == self.handle
            if error:
                raise OSError(error, 'NBD read failed')
            output += self.exact(length)
            self.bytes_read += length
            self.requests += 1
        return bytes(output[offset - start:offset - start + size])

    def close(self):
        try:
            self.sock.sendall(struct.pack('!IHHQQI', 0x25609513, 0, 2, 0, 0, 0))
        except OSError:
            pass
        finally:
            self.sock.close()


class Partition(io.RawIOBase):
    def __init__(self, disk, offset, size):
        self.disk, self.offset, self.size, self.position = disk, offset, size, 0

    def seek(self, offset, whence=0):
        position = offset if whence == 0 else self.position + offset if whence == 1 else self.size + offset if whence == 2 else -1
        if position < 0 or position > self.size:
            raise ValueError('Partition seek out of bounds')
        self.position = position
        return position

    def tell(self):
        return self.position

    def readable(self):
        return True

    def seekable(self):
        return True

    def read(self, size=-1):
        if size < 0:
            raise ValueError('Unbounded volume read refused')
        size = min(size, self.size - self.position)
        data = self.disk.read_at(self.offset + self.position, size)
        self.position += len(data)
        return data


def partitions(disk):
    header = disk.read_at(512, 512)
    assert header[:8] == b'EFI PART'
    header_size, header_crc = struct.unpack_from('<II', header, 12)
    checked = bytearray(header[:header_size])
    checked[16:20] = bytes(4)
    assert zlib.crc32(checked) == header_crc
    table_lba, count, entry_size, table_crc = struct.unpack_from('<QIII', header, 72)
    assert 0 < count <= 256 and 128 <= entry_size <= 512
    table = disk.read_at(table_lba * 512, count * entry_size)
    assert zlib.crc32(table) == table_crc
    result = []
    for index in range(count):
        entry = table[index * entry_size:(index + 1) * entry_size]
        if entry[:16] == bytes(16):
            continue
        first, last = struct.unpack_from('<QQ', entry, 32)
        result.append({'index': index, 'type': str(uuid.UUID(bytes_le=entry[:16])),
                       'offset': first * 512, 'size': (last - first + 1) * 512,
                       'name': entry[56:128].decode('utf-16le').rstrip('\0')})
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('output', type=Path)
    parser.add_argument('--layout-only', action='store_true')
    parser.add_argument('--diagnostics', action='store_true')
    parser.add_argument('--containers', action='store_true')
    parser.add_argument('--binary-log', action='append', default=[])
    args = parser.parse_args()
    for stem in args.binary_log:
        assert re.fullmatch(r'\d{3}-[A-Za-z][A-Za-z0-9_-]*', stem) and int(stem[:3]) < 142, 'Invalid native result-log stem'
    if not args.layout_only:
        from dissect.ntfs import NTFS
    lib = runpy.run_path(str(KIT / 'trial-vbsoff.py'), run_name='fixture_library')
    assert lib['current_pid']() == 22354, 'Fixture changed; no export created'
    assert not SOCKET.exists(), 'Existing export endpoint; inspect without replacing'
    args.output.mkdir(mode=0o700, exist_ok=False)
    monitor = Monitor()
    disk = None
    started = False
    record = {'capture_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'qemu_pid': 22354, 'scope': 'flushed disk bytes, not an atomic snapshot or native task-state query',
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'vm_stopped_or_restarted': False, 'files': []}
    try:
        block = monitor.command('info block')
        assert b'win-best-9711.qcow2' in block
        monitor.command('nbd_server_start unix:' + str(SOCKET))
        assert SOCKET.is_socket(), 'No new NBD endpoint created'
        started = True
        os.chmod(SOCKET, 0o600)
        monitor.command('nbd_server_add hd ' + EXPORT)
        disk = ReadOnlyNbd()
        record.update(export_read_only=bool(disk.flags & 2), export_flags=hex(disk.flags), disk_size=disk.size)
        layout = partitions(disk)
        record['partitions'] = layout
        print('Read-only flag verified; GPT CRCs valid', flush=True)
        if not args.layout_only:
            volumes = [p for p in layout if p['type'] == 'ebd0a0a2-b9e5-4433-87c0-68b6b72699c7'
                       and disk.read_at(p['offset'], 512)[3:11] == b'NTFS    ']
            assert len(volumes) == 1, 'Expected one Windows NTFS data volume'
            volume = volumes[0]
            ntfs = NTFS(Partition(disk, volume['offset'], volume['size']))
            paths = FILES + (CONTAINERS if args.containers or args.diagnostics else ()) + (DIAGNOSTICS if args.diagnostics else ())
            paths += tuple('shards-step10/full-real-04/' + stem + '.' + stream + '.txt'
                           for stem in args.binary_log for stream in ['stdout', 'stderr'])
            paths = tuple(dict.fromkeys(paths))
            for path in paths:
                entry = {'windows_path': 'C:/' + path}
                try:
                    source = ntfs.mft.get(path)
                    limit = (32 if path.endswith('.evtx') else 2) * 1048576
                    size = source.size()
                    assert size <= limit, 'Unexpected result file size'
                    stream = source.open()
                    parts = []
                    while sum(map(len, parts)) < size:
                        part = stream.read(min(1048576, size - sum(map(len, parts))))
                        if not part:
                            raise EOFError('File became shorter during live read')
                        parts.append(part)
                    data = b''.join(parts)
                    local = args.output / path.rsplit('/', 1)[-1]
                    local.write_bytes(data)
                    entry.update(bytes=len(data), sha256=hashlib.sha256(data).hexdigest(), local=local.name)
                    if path.endswith('/results.jsonl'):
                        rows = [json.loads(line) for line in data.decode('utf-8-sig').splitlines() if line.strip()]
                        entry.update(native_records=len(rows), failed_records=sum(row['exit'] != 0 for row in rows),
                                     last_records=[{'index': r['index'], 'name': r['name'], 'exit': r['exit']} for r in rows[-6:]])
                        print('Flushed native results:', entry, flush=True)
                    elif path.endswith('/full-real04-normal-controller.txt'):
                        text = data.decode('utf-16')
                        entry['last_heartbeats'] = [line for line in text.splitlines() if line.startswith('ALIVE ')][-4:]
                        print('Flushed controller heartbeats:', entry['last_heartbeats'], flush=True)
                    else:
                        print(path, len(data), 'flushed bytes retained', flush=True)
                except Exception as error:
                    entry.update(error_type=type(error).__name__, error=str(error))
                    print(path, entry['error_type'], entry['error'], flush=True)
                record['files'].append(entry)
        record.update(nbd_requests=disk.requests, nbd_bytes_read=disk.bytes_read)
    finally:
        if disk is not None:
            disk.close()
        if started:
            monitor.command('nbd_server_stop')
            probe = socket.socket(socket.AF_UNIX)
            probe.settimeout(1)
            try:
                probe.connect(str(SOCKET))
                record['export_stopped'] = False
                raise RuntimeError('NBD listener remains; inspect before another export')
            except (FileNotFoundError, ConnectionRefusedError):
                record['export_stopped'] = True
                SOCKET.unlink(missing_ok=True)
            finally:
                probe.close()
        (args.output / 'capture.json').write_text(json.dumps(record, indent=2) + '\n')
        (args.output / 'monitor.raw.txt').write_bytes(monitor.transcript)
        monitor.sock.close()
    print('Temporary export closed; VM and native task untouched', flush=True)


if __name__ == '__main__':
    main()
