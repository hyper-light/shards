# The bracketed lower bound of a lone thread's "other threads" CPU (D209 Part 5): python3 -I beside.py N SLEEP_S.
import time, sys
T, P = time.CLOCK_THREAD_CPUTIME_ID, time.CLOCK_PROCESS_CPUTIME_ID
rt = int(time.clock_getres(T)*1e9+0.5); rp = int(time.clock_getres(P)*1e9+0.5)
def read():
    a = time.clock_gettime_ns(T); p = time.clock_gettime_ns(P); b = time.clock_gettime_ns(T)
    return a, p, b
n = int(sys.argv[1]); sleep = float(sys.argv[2])
last = read(); worst = -10**18; over = 0; over1 = 0
for i in range(n):
    if sleep: time.sleep(sleep)
    now = read()
    lb = (now[1]-last[1]) - (now[2]-last[0])
    worst = max(worst, lb)
    if lb > rp + rt: over += 1
    if lb > 0: over1 += 1
    last = now
print(f"res thread {rt} ns process {rp} ns; n {n} sleep {sleep}: worst lower bound {worst} ns; > res sum: {over}; > 0: {over1}")
