# Finding 1: Fixture::start/shutdown compared the whole process's /proc/self/fd count
# (managed_output.rs:206, 297-311). Another test thread of the same process moves that
# count. Deterministic: barriers order the neighbor's open/close inside the window.
import os
import socket
import threading


def count():
    return len(os.listdir("/proc/self/fd"))


def case(name):
    baseline_taken, observer_closed = threading.Barrier(2), threading.Barrier(2)
    held = {}

    def neighbor():
        if name == "masked":
            held["f"] = open("/proc/self/stat", "rb")  # opened before the baseline
        baseline_taken.wait()
        observer_closed.wait()
        if name == "spurious":
            held["f"] = open("/proc/self/stat", "rb")  # opened inside the window
        if name == "masked":
            held.pop("f").close()  # closed inside the window

    thread = threading.Thread(target=neighbor)
    thread.start()
    baseline_taken.wait()
    before = count()
    # An owner observer: its QUIC socket and its waker, both UDP on loopback.
    observer = [socket.socket(socket.AF_INET, socket.SOCK_DGRAM) for _ in range(2)]
    for s in observer:
        s.bind(("127.0.0.1", 0))
    leaked = observer.pop() if name == "masked" else None
    for s in observer:
        s.close()
    observer_closed.wait()
    thread.join()
    after = count()
    verdict = "pass" if after == before else "FAIL"
    truth = "one observer socket leaked" if leaked else "every observer socket closed"
    print(f"case={name} before={before} after={after} count_check={verdict} truth={truth}")
    if leaked:
        leaked.close()
    if "f" in held:
        held.pop("f").close()


case("spurious")
case("masked")
