#!/usr/bin/env python3
"""One Linux OS-ordering observation; this is not a Shards protocol codec."""
from array import array
from pathlib import Path
import errno
import hashlib
import json
import os
import platform
import resource
import select
import socket
import sys
import time
import traceback

POLICY = '28ba5083bd65a9a3bff4896921821cbbd4f1fee7'
RULES = 'df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3'


def fd_snapshot():
    # listdir's transient directory FD is present in both snapshots; do not
    # interpret this observation as a kernel-reference or population proof.
    return sorted(int(name) for name in os.listdir('/proc/self/fd'))


def observe(trace):
    sockets = []
    owned_fds = set()
    original_fds = fd_snapshot()
    cleanup_errors = []
    try:
        older_read, older_write = os.pipe2(os.O_CLOEXEC | os.O_NONBLOCK)
        owned_fds.update((older_read, older_write))
        newer_read, newer_write = os.pipe2(os.O_CLOEXEC | os.O_NONBLOCK)
        owned_fds.update((newer_read, newer_write))
        sender, receiver = socket.socketpair(
            socket.AF_UNIX, socket.SOCK_SEQPACKET | socket.SOCK_CLOEXEC | socket.SOCK_NONBLOCK)
        sockets.extend((sender, receiver))
        headers = [b'fixture:older', b'fixture:newer', b'fixture:cut-through-newer']
        body_fds = [older_read, newer_read, None]
        for header, descriptor in zip(headers, body_fds):
            ancillary = [] if descriptor is None else [
                (socket.SOL_SOCKET, socket.SCM_RIGHTS, array('i', [descriptor]))]
            sent = sender.sendmsg([header], ancillary, socket.MSG_NOSIGNAL)
            assert sent == len(header)
            trace.append({'event': 'control_sent', 'header': header.decode(), 'bytes': sent,
                          'body_fd': descriptor})

        received_headers = []
        received_bodies = []
        for expected, original in zip(headers, body_fds):
            data, ancillary, flags, address = receiver.recvmsg(
                max(map(len, headers)), socket.CMSG_SPACE(array('i').itemsize),
                socket.MSG_CMSG_CLOEXEC)
            received = []
            for level, kind, payload in ancillary:
                assert level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS
                descriptors = array('i')
                descriptors.frombytes(payload)
                received.extend(descriptors)
                owned_fds.update(descriptors)
            assert not flags & (socket.MSG_TRUNC | socket.MSG_CTRUNC)
            assert data == expected
            assert len(received) == (0 if original is None else 1)
            received_headers.append(data)
            if original is not None:
                original_stat = os.fstat(original)
                received_stat = os.fstat(received[0])
                assert (original_stat.st_dev, original_stat.st_ino) == (
                    received_stat.st_dev, received_stat.st_ino)
                assert not os.get_inheritable(received[0])
                received_bodies.append(received[0])
            trace.append({'event': 'control_received', 'header': data.decode(),
                          'received_fds': received, 'flags': flags,
                          'body_object_matched': original is not None})
        assert received_headers == headers
        older_received, newer_received = received_bodies
        newer = b'newer-body'
        assert os.write(newer_write, newer) == len(newer)
        poller = select.poll()
        for descriptor in received_bodies:
            poller.register(descriptor, select.POLLIN | select.POLLHUP | select.POLLERR)
        ready = dict(poller.poll(0))  # One observation; no retry/probe loop.
        assert older_received not in ready
        assert ready.get(newer_received, 0) & select.POLLIN
        try:
            unexpected = os.read(older_received, len(b'older-body'))
        except BlockingIOError as error:
            assert error.errno == errno.EAGAIN
            older_state = {'state': 'pending-with-writer-held', 'saved_errno': error.errno}
        else:
            raise AssertionError(('older body unexpectedly readable/EOF', unexpected))
        assert os.read(newer_received, len(newer)) == newer
        trace.append({'event': 'later_control_and_body_observed', 'cut_received': True,
                      'older_body': older_state, 'newer_body': newer.decode(),
                      'readiness': ready})

        # The old publication source still exists after the cut was received.
        # Completing it now proves that the cut did not itself retire that source.
        older = b'older-body'
        assert os.write(older_write, older) == len(older)
        assert os.read(older_received, len(older)) == older
        trace.append({'event': 'older_body_completed_after_cut', 'body': older.decode()})
        for descriptor in [older_write, newer_write]:
            os.close(descriptor)
            owned_fds.remove(descriptor)
        assert os.read(older_received, len(older)) == b''
        assert os.read(newer_received, len(newer)) == b''
        trace.append({'event': 'actual_pipe_eof_after_writer_close'})
    finally:
        for descriptor in sorted(owned_fds):
            try:
                os.close(descriptor)
            except OSError as error:
                cleanup_errors.append({'fd': descriptor, 'errno': error.errno})
        for endpoint in sockets:
            endpoint.close()
        remaining_fds = fd_snapshot()
        trace.append({'event': 'owned_cleanup', 'before': original_fds,
                      'after': remaining_fds, 'errors': cleanup_errors})
    assert not cleanup_errors and remaining_fds == original_fds


def main():
    output = Path(sys.argv[1])
    output.mkdir()
    start = time.monotonic()
    trace = []
    failure = None
    identity = {'policy_revision': POLICY, 'rules_sha256': RULES, 'role': 'coordinator',
                'kernel': platform.release(), 'architecture': platform.machine(),
                'uid': os.getuid(), 'pid': os.getpid(),
                'affinity': sorted(os.sched_getaffinity(0)),
                'threads': len(os.listdir('/proc/self/task')),
                'cgroup': Path('/proc/self/cgroup').read_text(),
                'rlimit_as': resource.getrlimit(resource.RLIMIT_AS),
                'rlimit_nofile': resource.getrlimit(resource.RLIMIT_NOFILE),
                'source_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    (output / 'identity.json').write_text(json.dumps(identity, indent=2) + '\n')
    try:
        assert sys.platform == 'linux'
        observe(trace)
    except BaseException as error:
        failure = {'type': type(error).__name__, 'message': str(error)}
        (output / 'failure.stderr').write_text(traceback.format_exc())
    (output / 'trace.json').write_text(json.dumps(trace, indent=2) + '\n')
    result = {'exit': 0 if failure is None else 1, 'failure': failure,
              'elapsed_seconds': time.monotonic() - start,
              'scope': 'one-process OS socket/pipe observation; no Shards codec/helper/actor',
              'processes_spawned': 0, 'production_imports': 0, 'held_dispatches': 0,
              'whole_kernel_reference_return': 'Unproved',
              'observation': 'ordered control cut arrived before old independent body completion'
                             if failure is None else 'probe failed; retain first failure'}
    (output / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))
    return result['exit']


if __name__ == '__main__':
    sys.exit(main())
