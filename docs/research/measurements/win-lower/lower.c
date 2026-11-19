/* How soon Windows takes a CPU from a running thread lowered below a ready one, and gives it
 * to a ready thread raised above the running one: D191's priority governor
 * (crates/vmm/src/platform/windows_thread.rs) raises and lowers spinning threads at its looks
 * and charges their pool what they ran raised, so what a lowered thread runs before Windows
 * applies its lowering is time no pool is charged.
 *
 * R spins at the priority the controller sets, its boost off (as the governor's test sets its
 * threads'); N spins at THREAD_PRIORITY_NORMAL; both pinned to the process's highest CPU. The
 * controller, pinned to its lowest at THREAD_PRIORITY_TIME_CRITICAL, alternates: a
 * pseudo-random 10 to 40 ms, R raised to ABOVE_NORMAL (timed until R runs), as long again, R
 * lowered (timed until N runs). So each change lands at any phase of the clock's tick. The
 * lowering, by arm, interleaved round by round:
 *   below    SetThreadPriority(R, BELOW_NORMAL), as the governor lowers;
 *   suspend  the same, then SuspendThread(R), GetThreadContext(R) (which returns once R is
 *            suspended) and ResumeThread(R);
 *   normal   SetThreadPriority(R, NORMAL), as the governor lowered until 2026-11-18 (a tenth
 *            of the rounds: N may wait a quantum of R's or more).
 * A spinner resumed at its first loop after one more than GAP_US before it. Also read: how
 * long R ran on past the lowering (its last loop before N ran), and what GetThreadTimes
 * charged R across it, in its 15.625 ms steps.
 *
 * Usage: lower.exe [ROUNDS]     ROUNDS of each of below and suspend (default 300)
 * Build: zig cc -O2 -target x86_64-windows-gnu lower.c -o lower.exe -ladvapi32
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifndef CREATE_WAITABLE_TIMER_HIGH_RESOLUTION
#define CREATE_WAITABLE_TIMER_HIGH_RESOLUTION 0x00000002
#endif

#define GAP_US 100
#define TIMEOUT_US 3000000
#define STEP_100NS 156250

static LONGLONG freq, gap;
static DWORD_PTR x_mask;
static volatile LONGLONG n_resumed, n_last, r_resumed, r_last;
static volatile LONG quit;
static HANDLE timer;
static unsigned seed = 12345;

static void die(const char *what) {
    fprintf(stderr, "%s: error %lu\n", what, GetLastError());
    exit(1);
}

static LONGLONG now(void) {
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return t.QuadPart;
}

static double us(LONGLONG ticks) { return (double)ticks * 1e6 / (double)freq; }

/* A thread's kernel and user time, in 100 ns. */
static ULONGLONG thread_time(HANDLE h) {
    FILETIME c, e, k, u;
    if (!GetThreadTimes(h, &c, &e, &k, &u)) die("GetThreadTimes");
    return (((ULONGLONG)k.dwHighDateTime << 32) | k.dwLowDateTime) +
           (((ULONGLONG)u.dwHighDateTime << 32) | u.dwLowDateTime);
}

static DWORD WINAPI spin(LPVOID arg) {
    volatile LONGLONG *resumed = arg ? &r_resumed : &n_resumed;
    volatile LONGLONG *last = arg ? &r_last : &n_last;
    LONGLONG prev = now();
    *resumed = prev;
    while (!quit) {
        LONGLONG t = now();
        if (t - prev > gap) *resumed = t;
        *last = t;
        prev = t;
    }
    return 0;
}

static HANDLE start(int raised, int priority) {
    HANDLE h = CreateThread(NULL, 0, spin, raised ? (LPVOID)1 : NULL, CREATE_SUSPENDED, NULL);
    if (!h) die("CreateThread");
    if (!SetThreadAffinityMask(h, x_mask)) die("SetThreadAffinityMask");
    if (!SetThreadPriority(h, priority)) die("SetThreadPriority");
    if (raised && !SetThreadPriorityBoost(h, TRUE)) die("SetThreadPriorityBoost");
    if (ResumeThread(h) == (DWORD)-1) die("ResumeThread");
    return h;
}

static void wait_us(LONGLONG usec) {
    LARGE_INTEGER due;
    due.QuadPart = -usec * 10;
    if (!SetWaitableTimer(timer, &due, 0, NULL, NULL, FALSE)) die("SetWaitableTimer");
    if (WaitForSingleObject(timer, INFINITE) != WAIT_OBJECT_0) die("WaitForSingleObject");
}

static LONGLONG pause_us(void) {
    seed = seed * 1103515245u + 12345u;
    return 10000 + (LONGLONG)((seed >> 8) % 30001u);
}

/* Spins until *v is past `after`: its value, or 0 at the timeout. */
static LONGLONG until(volatile LONGLONG *v, LONGLONG after) {
    LONGLONG deadline = now() + (LONGLONG)TIMEOUT_US * freq / 1000000;
    for (;;) {
        LONGLONG x = *v;
        if (x > after) return x;
        if (now() > deadline) return 0;
        YieldProcessor();
    }
}

enum { BELOW, SUSPEND, NORMAL, ARMS };
static const char *names[ARMS] = {"below", "suspend", "normal"};

typedef struct {
    double *lower, *ran_on, *raise;
    int steps[4];
    int n, raises, timeouts;
} Arm;

static void round_of(HANDLE r, int arm, Arm *a) {
    /* R lowered, N running: raise R. */
    wait_us(pause_us());
    LONGLONG t1 = now();
    if (!SetThreadPriority(r, THREAD_PRIORITY_ABOVE_NORMAL)) die("raise");
    if (!until(&r_last, t1)) {
        a->timeouts++;
        return;
    }
    LONGLONG rr = r_resumed;
    a->raise[a->raises++] = rr > t1 ? us(rr - t1) : 0.0;
    /* R running, N waiting: lower R. */
    wait_us(pause_us());
    ULONGLONG before = thread_time(r);
    LONGLONG t0 = now();
    if (!SetThreadPriority(r, arm == NORMAL ? THREAD_PRIORITY_NORMAL : THREAD_PRIORITY_BELOW_NORMAL))
        die("lower");
    if (arm == SUSPEND) {
        CONTEXT ctx;
        if (SuspendThread(r) == (DWORD)-1) die("SuspendThread");
        memset(&ctx, 0, sizeof ctx);
        ctx.ContextFlags = CONTEXT_CONTROL;
        if (!GetThreadContext(r, &ctx)) die("GetThreadContext");
        if (ResumeThread(r) == (DWORD)-1) die("ResumeThread");
    }
    LONGLONG nr = until(&n_resumed, t0);
    if (!nr) {
        a->timeouts++;
    } else {
        ULONGLONG charged = thread_time(r) - before;
        int steps = (int)((charged + STEP_100NS / 2) / STEP_100NS);
        a->lower[a->n] = us(nr - t0);
        a->ran_on[a->n] = r_last > t0 ? us(r_last - t0) : 0.0;
        a->steps[steps > 3 ? 3 : steps]++;
        a->n++;
    }
    /* Leave R below N, and N running, for the next round. */
    if (arm == NORMAL) {
        LONGLONG t2 = now();
        if (!SetThreadPriority(r, THREAD_PRIORITY_BELOW_NORMAL)) die("lower again");
        if (!until(&n_last, t2)) die("N never ran again");
    }
}

static int cmp(const void *x, const void *y) {
    double a = *(const double *)x, b = *(const double *)y;
    return (a > b) - (a < b);
}

/* Nearest rank. */
static double pct(double *v, int n, double q) {
    int k = (int)(q * n + 0.999999);
    if (k < 1) k = 1;
    if (k > n) k = n;
    return v[k - 1];
}

static void line(const char *arm, const char *what, double *v, int n) {
    if (n == 0) {
        printf("| %s | %s | 0 | | | | |\n", arm, what);
        return;
    }
    qsort(v, (size_t)n, sizeof *v, cmp);
    printf("| %s | %s | %d | %.1f | %.1f | %.1f | %.1f |\n", arm, what, n, pct(v, n, 0.5), pct(v, n, 0.9),
           pct(v, n, 0.99), v[n - 1]);
}

typedef LONG(NTAPI *QueryTimerResolution)(PULONG, PULONG, PULONG);
typedef LONG(NTAPI *GetVersion_)(OSVERSIONINFOW *);

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 300;
    LARGE_INTEGER f;
    DWORD_PTR process, system;
    if (rounds < 1) rounds = 1;
    QueryPerformanceFrequency(&f);
    freq = f.QuadPart;
    gap = (LONGLONG)GAP_US * freq / 1000000;
    if (!GetProcessAffinityMask(GetCurrentProcess(), &process, &system)) die("GetProcessAffinityMask");
    if ((process & (process - 1)) == 0) {
        fprintf(stderr, "one CPU: the controller needs one of its own\n");
        return 1;
    }
    {
        int hi = 63;
        while (!(process >> hi & 1)) hi--;
        x_mask = (DWORD_PTR)1 << hi;
    }
    HMODULE ntdll = GetModuleHandleA("ntdll.dll");
    OSVERSIONINFOW os;
    memset(&os, 0, sizeof os);
    os.dwOSVersionInfoSize = sizeof os;
    GetVersion_ rtl = (GetVersion_)(void *)GetProcAddress(ntdll, "RtlGetVersion");
    if (rtl) rtl(&os);
    ULONG coarsest = 0, finest = 0, current = 0;
    QueryTimerResolution qtr = (QueryTimerResolution)(void *)GetProcAddress(ntdll, "NtQueryTimerResolution");
    if (qtr) qtr(&coarsest, &finest, &current);
    DWORD sep = 0, size = sizeof sep;
    LONG got = RegGetValueA(HKEY_LOCAL_MACHINE, "SYSTEM\\CurrentControlSet\\Control\\PriorityControl",
                            "Win32PrioritySeparation", RRF_RT_REG_DWORD, NULL, &sep, &size);
    printf("Windows %lu.%lu.%lu, process CPUs mask %#llx, X %#llx, QPC %lld Hz\n", os.dwMajorVersion,
           os.dwMinorVersion, os.dwBuildNumber, (unsigned long long)process, (unsigned long long)x_mask,
           (long long)freq);
    printf("timer resolution (100 ns): coarsest %lu finest %lu current %lu; Win32PrioritySeparation %s%#lx\n",
           coarsest, finest, current, got == ERROR_SUCCESS ? "" : "unread ", (unsigned long)sep);

    timer = CreateWaitableTimerExW(NULL, NULL, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS);
    if (!timer) die("CreateWaitableTimerExW(HIGH_RESOLUTION)");
    if (!SetThreadAffinityMask(GetCurrentThread(), process & (~process + 1))) die("pin controller");
    if (!SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL)) die("controller priority");
    HANDLE n = start(0, THREAD_PRIORITY_NORMAL);
    HANDLE r = start(1, THREAD_PRIORITY_BELOW_NORMAL);
    (void)n;
    wait_us(100000);

    int each[ARMS] = {rounds, rounds, rounds / 10 > 0 ? rounds / 10 : 1};
    Arm arms[ARMS];
    for (int a = 0; a < ARMS; a++) {
        memset(&arms[a], 0, sizeof arms[a]);
        arms[a].lower = calloc((size_t)each[a], sizeof(double));
        arms[a].ran_on = calloc((size_t)each[a], sizeof(double));
        arms[a].raise = calloc((size_t)each[a], sizeof(double));
        if (!arms[a].lower || !arms[a].ran_on || !arms[a].raise) die("calloc");
    }
    for (int i = 0; i < rounds; i++) {
        round_of(r, BELOW, &arms[BELOW]);
        round_of(r, SUSPEND, &arms[SUSPEND]);
        if (i % 10 == 9 || (rounds < 10 && i == 0)) round_of(r, NORMAL, &arms[NORMAL]);
    }
    quit = 1;

    printf("\nThe first 40 lowerings of arm below, us until N ran: ");
    for (int i = 0; i < arms[BELOW].n && i < 40; i++) printf("%.0f ", arms[BELOW].lower[i]);
    printf("\n\n| arm | us | n | p50 | p90 | p99 | max |\n|---|---|---|---|---|---|---|\n");
    for (int a = 0; a < ARMS; a++) {
        line(names[a], "lowered until N ran", arms[a].lower, arms[a].n);
        line(names[a], "R ran on past it", arms[a].ran_on, arms[a].n);
        line(names[a], "raised until R ran", arms[a].raise, arms[a].raises);
    }
    printf("\nGetThreadTimes steps charged R across the lowering (0 / 1 / 2 / 3+), and timeouts (%d s):\n",
           TIMEOUT_US / 1000000);
    for (int a = 0; a < ARMS; a++)
        printf("  %-8s %d / %d / %d / %d; timeouts %d\n", names[a], arms[a].steps[0], arms[a].steps[1],
               arms[a].steps[2], arms[a].steps[3], arms[a].timeouts);
    return 0;
}
