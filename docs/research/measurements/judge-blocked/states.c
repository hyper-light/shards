// Where a command's wall time goes, by its threads' run states as the judge's looks read
// them (D215, crates/api progress.rs `os::hold`, `os::threads_cpu`): PROC_PIDTHREADINFO's
// pth_run_state (sys/proc_info.h, mach/thread_info.h) sampled every SAMPLE_US, with the
// process's CPU (proc_pid_rusage(2) RUSAGE_INFO_V0, Mach units by mach_timebase_info(3)).
// The process's state at a sample is its most telling thread's: RUNNING (running or
// waiting for a CPU) where one is, else UNINTERRUPTIBLE (a wait in the kernel), else
// WAITING (an interruptible wait), else STOPPED. Prints each run of one state 1 ms or
// longer, then the wall and the CPU in each state, and the command's exit.
//
//   cc -O2 -o states states.c && ./states COMMAND [ARGS...]
#include <libproc.h>
#include <mach/mach_time.h>
#include <mach/thread_info.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/proc_info.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

#define SAMPLE_US 100

static mach_timebase_info_data_t tb;

static uint64_t ns(uint64_t ticks) { return ticks * tb.numer / tb.denom; }

static uint64_t cpu(pid_t pid) {
    struct rusage_info_v0 ri;
    if (proc_pid_rusage(pid, RUSAGE_INFO_V0, (rusage_info_t *)&ri) != 0) return 0;
    return ns(ri.ri_user_time + ri.ri_system_time);
}

static int state(pid_t pid) {
    uint64_t threads[512];
    int got = proc_pidinfo(pid, PROC_PIDLISTTHREADS, 0, threads, sizeof threads);
    if (got <= 0) return 0;
    int seen[8] = {0};
    for (int i = 0; i < got / (int)sizeof(uint64_t); i++) {
        struct proc_threadinfo info;
        if (proc_pidinfo(pid, PROC_PIDTHREADINFO, threads[i], &info, sizeof info) == sizeof info &&
            info.pth_run_state > 0 && info.pth_run_state < 8)
            seen[info.pth_run_state]++;
    }
    if (seen[TH_STATE_RUNNING]) return TH_STATE_RUNNING;
    if (seen[TH_STATE_UNINTERRUPTIBLE]) return TH_STATE_UNINTERRUPTIBLE;
    if (seen[TH_STATE_WAITING]) return TH_STATE_WAITING;
    if (seen[TH_STATE_STOPPED]) return TH_STATE_STOPPED;
    return 0;
}

static const char *named(int s) {
    switch (s) {
    case TH_STATE_RUNNING: return "running";
    case TH_STATE_UNINTERRUPTIBLE: return "uninterruptible";
    case TH_STATE_WAITING: return "waiting";
    case TH_STATE_STOPPED: return "stopped";
    default: return "unread";
    }
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: states COMMAND [ARGS...]\n");
        return 2;
    }
    mach_timebase_info(&tb);
    uint64_t began = ns(mach_absolute_time());
    pid_t pid;
    if (posix_spawnp(&pid, argv[1], NULL, NULL, argv + 1, environ) != 0) {
        perror("posix_spawnp");
        return 2;
    }
    uint64_t wall[8] = {0}, used[8] = {0};
    int run = -1;
    uint64_t run_at = 0, run_cpu = 0, last_at = 0, last_cpu = 0;
    siginfo_t info;
    for (;;) {
        memset(&info, 0, sizeof info);
        if (waitid(P_PID, (id_t)pid, &info, WEXITED | WNOHANG | WNOWAIT) == 0 && info.si_pid != 0) break;
        int s = state(pid);
        uint64_t at = ns(mach_absolute_time()) - began, c = cpu(pid);
        if (run >= 0) {
            wall[run] += at - last_at;
            used[run] += c - last_cpu;
        }
        if (s != run) {
            if (run >= 0 && last_at - run_at >= 1000000)
                printf("%9.3f ms  %-15s %8.3f ms wall %8.3f ms CPU\n", run_at / 1e6, named(run),
                       (last_at - run_at) / 1e6, (last_cpu - run_cpu) / 1e6);
            run = s, run_at = at, run_cpu = c;
        }
        last_at = at, last_cpu = c;
        usleep(SAMPLE_US);
    }
    uint64_t end = ns(mach_absolute_time()) - began, total = cpu(pid);
    if (run >= 0 && last_at - run_at >= 1000000)
        printf("%9.3f ms  %-15s %8.3f ms wall %8.3f ms CPU\n", run_at / 1e6, named(run),
               (last_at - run_at) / 1e6, (last_cpu - run_cpu) / 1e6);
    int status = 0;
    waitpid(pid, &status, 0);
    printf("total %.3f ms wall, %.3f ms CPU, exit %d;", end / 1e6, total / 1e6, WEXITSTATUS(status));
    for (int s = 1; s < 8; s++)
        if (wall[s]) printf(" %s %.3f ms wall %.3f ms CPU;", named(s), wall[s] / 1e6, used[s] / 1e6);
    printf("\n");
    return 0;
}
