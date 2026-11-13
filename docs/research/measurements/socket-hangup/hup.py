# PM M559 (D251): what a Unix stream socket says at its end, a peer's half-close against its
# going, before this side writes. For each case a connected AF_UNIX stream pair: poll(2) asked
# for POLLOUT with a timeout of 0, a recv that does not wait, poll again, then a send of one
# byte. Run: python3 -I hup.py (macOS and Linux).
import socket, select, sys, platform
P = {select.POLLIN:'IN', select.POLLOUT:'OUT', select.POLLHUP:'HUP', select.POLLERR:'ERR'}
def flags(r): return '|'.join(n for b,n in P.items() if r & b) or '0'
def probe(a):
    p = select.poll(); p.register(a.fileno(), select.POLLOUT)
    ev = p.poll(0)
    return flags(ev[0][1]) if ev else '0'
def recv(a):
    try: return repr(a.recv(16, socket.MSG_DONTWAIT))
    except BlockingIOError: return 'EAGAIN'
    except OSError as e: return 'errno %d' % e.errno
def send(a):
    try: return 'sent %d' % a.send(b'x')
    except OSError as e: return 'errno %d' % e.errno

def case(name, act):
    a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    keep = act(a, b)
    out = 'pollout=%-12s recv=%-10s' % (probe(a), recv(a))
    out += ' pollout-after-recv=%-10s send=%s' % (probe(a), send(a))
    print('%-46s %s' % (name, out)); sys.stdout.flush()
case('peer shutdown(SHUT_WR) (half-close)', lambda a,b: b.shutdown(socket.SHUT_WR) or b)
case('peer close() (cut)', lambda a,b: b.close())
case('peer close() with our byte unread (cut)', lambda a,b: (a.send(b'q'), b.close()))
case('peer shutdown(SHUT_RDWR)', lambda a,b: b.shutdown(socket.SHUT_RDWR) or b)
case('we shut WR, then peer half-closes', lambda a,b: (a.shutdown(socket.SHUT_WR), b.shutdown(socket.SHUT_WR), b)[2])
case('we shut WR, then peer closes', lambda a,b: (a.shutdown(socket.SHUT_WR), b.close()))
case('open, nothing ended', lambda a,b: b)
print(platform.system(), platform.release(), platform.machine())
