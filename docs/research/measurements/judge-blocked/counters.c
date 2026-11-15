// Whether macOS's accumulated per-process counters show a wait in the kernel that falls
// between two looks (D215's vouch): runs a command, samples its threads' run states every
// SAMPLE_US (PROC_PIDTHREADINFO pth_run_state, as states.c) with proc_pid_rusage(2)
// RUSAGE_INFO_V4's ri_pageins, ri_diskio_bytesread, ri_diskio_byteswritten and
// ri_logical_writes, and says, of each stretch of 1 ms or more in which a thread waited
// uninterruptibly (TH_STATE_UNINTERRUPTIBLE), how much each counter grew from the sample
// before it to the first sample after it; then how many such stretches, and how much of
// their time, saw any counter grow.
//
//   cc -O2 -o counters counters.c && ./counters COMMAND [ARGS...]
//   ./counters flush N   # N times: 4 KiB written to ./flushed, then F_FULLFSYNC
//   ./counters dirsync N # N times: a file created in ./d, then F_FULLFSYNC of ./d
#include <fcntl.h>
#include <libproc.h>
#include <mach/mach_time.h>
#include <mach/thread_info.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/proc_info.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

#define SAMPLE_US 100

static mach_timebase_info_data_t tb;
static uint64_t ns(uint64_t ticks) { return ticks * tb.numer / tb.denom; }

struct counts { uint64_t pageins, read, written, logical; };

static int counted(pid_t pid, struct counts *c) {
    struct rusage_info_v4 ri;
    if (proc_pid_rusage(pid, RUSAGE_INFO_V4, (rusage_info_t *)&ri) != 0) return 0;
    c->pageins = ri.ri_pageins, c->read = ri.ri_diskio_bytesread;
    c->written = ri.ri_diskio_byteswritten, c->logical = ri.ri_logical_writes;
    return 1;
}

static int uninterruptible(pid_t pid) {
    uint64_t threads[512];
    int got = proc_pidinfo(pid, PROC_PIDLISTTHREADS, 0, threads, sizeof threads);
    for (int i = 0; got > 0 && i < got / (int)sizeof(uint64_t); i++) {
        struct proc_threadinfo info;
        if (proc_pidinfo(pid, PROC_PIDTHREADINFO, threads[i], &info, sizeof info) == sizeof info &&
            info.pth_run_state == TH_STATE_UNINTERRUPTIBLE)
            return 1;
    }
    return 0;
}

static int work(const char *mode, int n) {
    char buf[4096];
    memset(buf, 1, sizeof buf);
    if (strcmp(mode, "flush") == 0) {
        int fd = open("flushed", O_CREAT | O_WRONLY | O_TRUNC, 0600);
        for (int i = 0; fd >= 0 && i < n; i++)
            if (write(fd, buf, sizeof buf) != sizeof buf || fcntl(fd, F_FULLFSYNC) != 0) return 1;
        return fd < 0;
    }
    mkdir("d", 0700);
    int dir = open("d", O_RDONLY);
    for (int i = 0; dir >= 0 && i < n; i++) {
        char name[64];
        snprintf(name, sizeof name, "d/f%d", i);
        int fd = open(name, O_CREAT | O_WRONLY, 0600);
        if (fd < 0 || close(fd) != 0 || fcntl(dir, F_FULLFSYNC) != 0) return 1;
    }
    return dir < 0;
}

int main(int argc, char **argv) {
    if (argc == 3 && (strcmp(argv[1], "flush") == 0 || strcmp(argv[1], "dirsync") == 0) && getenv("COUNTERS_CHILD"))
        return work(argv[1], atoi(argv[2]));
    if (argc < 2) {
        fprintf(stderr, "usage: counters COMMAND [ARGS...] | flush N | dirsync N\n");
        return 2;
    }
    mach_timebase_info(&tb);
    char **cmd = argv + 1;
    char *self[] = {argv[0], argv[1], argc > 2 ? argv[2] : "100", NULL};
    if (strcmp(argv[1], "flush") == 0 || strcmp(argv[1], "dirsync") == 0) {
        setenv("COUNTERS_CHILD", "1", 1);
        cmd = self;
    }
    pid_t pid;
    if (posix_spawnp(&pid, cmd[0], NULL, NULL, cmd, environ) != 0) {
        perror("posix_spawnp");
        return 2;
    }
    unsetenv("COUNTERS_CHILD");
    struct counts before = {0}, now = {0}, at_start = {0};
    int in = 0, stretches = 0, grown = 0;
    uint64_t began = 0, held = 0, held_grown = 0, t0 = ns(mach_absolute_time());
    siginfo_t info;
    for (;;) {
        memset(&info, 0, sizeof info);
        if (waitid(P_PID, (id_t)pid, &info, WEXITED | WNOHANG | WNOWAIT) == 0 && info.si_pid != 0) break;
        int u = uninterruptible(pid);
        uint64_t at = ns(mach_absolute_time()) - t0;
        if (!counted(pid, &now)) break;
        if (u && !in) in = 1, began = at, at_start = before;
        if (!u && in) {
            in = 0;
            uint64_t len = at - began;
            if (len >= 1000000) {
                int g = now.pageins > at_start.pageins || now.read > at_start.read ||
                        now.written > at_start.written || now.logical > at_start.logical;
                stretches++, grown += g, held += len, held_grown += g ? len : 0;
                printf("%9.3f ms uninterruptible %7.3f ms: pageins +%llu, read +%llu B, written +%llu B, logical +%llu B\n",
                       began / 1e6, len / 1e6, now.pageins - at_start.pageins, now.read - at_start.read,
                       now.written - at_start.written, now.logical - at_start.logical);
            }
        }
        before = now;
        usleep(SAMPLE_US);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    printf("exit %d; %d stretches of 1 ms or more uninterruptible, %.3f ms; %d of them, %.3f ms, saw a counter grow\n",
           WEXITSTATUS(status), stretches, held / 1e6, grown, held_grown / 1e6);
    return 0;
}
