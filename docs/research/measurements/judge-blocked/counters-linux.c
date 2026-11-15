// counters.c's Linux twin: whether Linux's accumulated per-process counters show a wait in
// the kernel that falls between two looks (D215's vouch). Runs a child that writes and
// fsyncs, samples its tasks' states every SAMPLE_US (/proc/<pid>/task/<tid>/stat field 3,
// proc(5)) with, summed over its tasks, field 12 (majflt) and field 42
// (delayacct_blkio_ticks, clock ticks, kept only with task delay accounting on), and
// /proc/<pid>/io's read_bytes and write_bytes, and says, of each stretch of 1 ms or more in
// which a task of it was in `D`, how much each grew from the sample before it to the first
// sample after it; then how many such stretches, and how much of their time, saw any grow.
//
//   cc -O2 -o counters-linux counters-linux.c && ./counters-linux SIZE N
//   N times: SIZE bytes written to ./flushed, then fdatasync(2).
#define _GNU_SOURCE
#include <dirent.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define SAMPLE_US 100

struct counts { uint64_t majflt, blkio, read, written; };

static uint64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
}

// Its tasks' states and counters; 1 where one is in D.
static int sample(pid_t pid, struct counts *c) {
    char path[128], stat[4096];
    int d = 0;
    memset(c, 0, sizeof *c);
    snprintf(path, sizeof path, "/proc/%d/task", pid);
    DIR *tasks = opendir(path);
    if (!tasks) return -1;
    struct dirent *e;
    while ((e = readdir(tasks))) {
        if (e->d_name[0] == '.') continue;
        snprintf(path, sizeof path, "/proc/%d/task/%s/stat", pid, e->d_name);
        FILE *f = fopen(path, "r");
        if (!f) continue;
        size_t n = fread(stat, 1, sizeof stat - 1, f);
        fclose(f);
        stat[n] = 0;
        char *p = strrchr(stat, ')');
        if (!p) continue;
        // Fields from 3 on, after the command's last ')'.
        char *field[64];
        int k = 0;
        for (char *t = strtok(p + 1, " "); t && k < 64; t = strtok(NULL, " ")) field[k++] = t;
        if (k < 40) continue;
        d |= field[0][0] == 'D';
        c->majflt += strtoull(field[12 - 3], NULL, 10);
        c->blkio += strtoull(field[42 - 3], NULL, 10);
    }
    closedir(tasks);
    snprintf(path, sizeof path, "/proc/%d/io", pid);
    FILE *io = fopen(path, "r");
    char line[256];
    while (io && fgets(line, sizeof line, io)) {
        unsigned long long v;
        if (sscanf(line, "read_bytes: %llu", &v) == 1) c->read = v;
        if (sscanf(line, "write_bytes: %llu", &v) == 1) c->written = v;
    }
    if (io) fclose(io);
    return d;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: counters-linux SIZE N\n");
        return 2;
    }
    size_t size = strtoull(argv[1], NULL, 10);
    int n = atoi(argv[2]);
    pid_t pid = fork();
    if (pid == 0) {
        char *buf = malloc(size);
        memset(buf, 1, size);
        int fd = open("flushed", O_CREAT | O_WRONLY | O_TRUNC, 0600);
        for (int i = 0; fd >= 0 && i < n; i++)
            if (write(fd, buf, size) != (ssize_t)size || fdatasync(fd) != 0) _exit(1);
        _exit(fd < 0);
    }
    struct counts before = {0}, now = {0}, at_start = {0};
    int in = 0, stretches = 0, grown = 0;
    uint64_t began = 0, held = 0, held_grown = 0, t0 = now_ns();
    for (;;) {
        siginfo_t info;
        memset(&info, 0, sizeof info);
        if (waitid(P_PID, (id_t)pid, &info, WEXITED | WNOHANG | WNOWAIT) == 0 && info.si_pid != 0) break;
        int d = sample(pid, &now);
        uint64_t at = now_ns() - t0;
        if (d < 0) break;
        if (d && !in) in = 1, began = at, at_start = before;
        if (!d && in) {
            in = 0;
            uint64_t len = at - began;
            if (len >= 1000000) {
                int g = now.majflt > at_start.majflt || now.blkio > at_start.blkio ||
                        now.read > at_start.read || now.written > at_start.written;
                stretches++, grown += g, held += len, held_grown += g ? len : 0;
                printf("%9.3f ms D %7.3f ms: majflt +%llu, blkio +%llu ticks, read +%llu B, written +%llu B\n",
                       began / 1e6, len / 1e6, (unsigned long long)(now.majflt - at_start.majflt),
                       (unsigned long long)(now.blkio - at_start.blkio),
                       (unsigned long long)(now.read - at_start.read),
                       (unsigned long long)(now.written - at_start.written));
            }
        }
        before = now;
        usleep(SAMPLE_US);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    printf("exit %d; %d stretches of 1 ms or more in D, %.3f ms; %d of them, %.3f ms, saw a counter grow; "
           "blkio ticks by its end %llu\n",
           WEXITSTATUS(status), stretches, held / 1e6, grown, held_grown / 1e6, (unsigned long long)now.blkio);
    return 0;
}
