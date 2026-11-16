// counters-linux.c's question for the daemon's removal (D215, D275): of the waits in the kernel
// of a process removing a microVM's directory as daemon.rs `complete_batch` does, beside another
// doing the same (two daemons' `finish`, nesting.rs's `Home::drop`), how many and how long show
// none of what the judge reads on Linux (progress.rs `Hold`: R state, run-queue wait, majflt,
// /proc/<pid>/io read_bytes and write_bytes, field 42 only with task delay accounting on)?
// Each child, N times, in a directory of its own under the working directory: makes ID/, writes
// a 4 KiB record in it, renames ID/ to .ID.removing (Removal::set_aside), fsyncs the directory
// (Removal::sync, Disk::sync_dir), and deletes .ID.removing (Removal::delete_all). The parent
// samples both children every SAMPLE_US: each stretch of 1 ms or more in which a task of a
// child was in D, and whether any count of that child grew from the sample before the stretch
// to the first after it. Prints the stretches no count showed (the judge's "still" before
// D215's amendment of 2026-11-16), n, p50, p90, p99 and max of their lengths, and the
// sysctls that decide what else it could read. Then, of those, the ones in which the host's
// block devices completed no I/O between the stretch's first and last samples in D
// (/proc/diskstats' reads, writes, discards and flushes completed, every line summed, as
// progress.rs `os::disks_completed` reads them): the judge's "still" since. Red is the first
// set holding stretches past the stops' W*; green is the second empty of them.
//
//   cc -O2 -o dirsync-linux dirsync-linux.c && ./dirsync-linux N
//
// Run in a directory on the filesystem under test (btrfs for the member-share stall), never
// a tmpfs, whose fsync waits on nothing.
#define _GNU_SOURCE
#include <dirent.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define SAMPLE_US 100
#define MAX_STRETCHES 100000

struct counts { uint64_t majflt, blkio, read, written, delay; int runnable; };

static uint64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
}

static uint64_t read_u64(const char *path, int field) {
    char text[256];
    FILE *f = fopen(path, "r");
    if (!f) return 0;
    size_t n = fread(text, 1, sizeof text - 1, f);
    fclose(f);
    text[n] = 0;
    char *t = strtok(text, " ");
    for (int i = 1; t && i < field; i++) t = strtok(NULL, " ");
    return t ? strtoull(t, NULL, 10) : 0;
}

// Its tasks' states and the counts the judge reads; 1 where a task is in D.
static int sample(pid_t pid, struct counts *c) {
    char path[160], stat[4096];
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
        char *field[64];
        int k = 0;
        for (char *t = strtok(p + 1, " "); t && k < 64; t = strtok(NULL, " ")) field[k++] = t;
        if (k < 40) continue;
        d |= field[0][0] == 'D';
        c->runnable |= field[0][0] == 'R';
        c->majflt += strtoull(field[12 - 3], NULL, 10);
        c->blkio += strtoull(field[42 - 3], NULL, 10);
        snprintf(path, sizeof path, "/proc/%d/task/%s/schedstat", pid, e->d_name);
        c->delay += read_u64(path, 2);
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

static void remover(const char *root, int n) {
    char dir[256], file[300], aside[256];
    static char record[4096];
    memset(record, 'r', sizeof record);
    mkdir(root, 0700);
    int rootfd = open(root, O_RDONLY | O_DIRECTORY);
    if (rootfd < 0) _exit(1);
    for (int i = 0; i < n; i++) {
        snprintf(dir, sizeof dir, "%s/%08d", root, i);
        snprintf(file, sizeof file, "%s/container.json", dir);
        snprintf(aside, sizeof aside, "%s/.%08d.removing", root, i);
        if (mkdir(dir, 0700) != 0) _exit(1);
        int fd = open(file, O_WRONLY | O_CREAT | O_TRUNC, 0600);
        if (fd < 0 || write(fd, record, sizeof record) != (ssize_t)sizeof record) _exit(1);
        close(fd);
        if (rename(dir, aside) != 0) _exit(1);
        if (fsync(rootfd) != 0) _exit(1);
        snprintf(file, sizeof file, "%s/container.json", aside);
        if (unlink(file) != 0 || rmdir(aside) != 0) _exit(1);
    }
    _exit(0);
}

// The host's block devices' completed I/O so far: each /proc/diskstats line's fields 1, 5, 12
// and 16 after its major, minor and name (Documentation/admin-guide/iostats.rst), summed.
static uint64_t disks(void) {
    FILE *f = fopen("/proc/diskstats", "r");
    if (!f) return 0;
    char line[512];
    uint64_t sum = 0;
    while (fgets(line, sizeof line, f)) {
        char *save = NULL;
        int column = 0;
        for (char *t = strtok_r(line, " \t\n", &save); t; t = strtok_r(NULL, " \t\n", &save)) {
            int stat = ++column - 3;
            if (stat == 1 || stat == 5 || stat == 12 || stat == 16) sum += strtoull(t, NULL, 10);
        }
    }
    fclose(f);
    return sum;
}

static void report(const char *what, uint64_t *v, size_t n, uint64_t ns);

static int by_value(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return (x > y) - (x < y);
}

static uint64_t rank(const uint64_t *v, size_t n, double q) {
    if (!n) return 0;
    size_t i = (size_t)(q * (double)n + 0.999999);
    return v[(i ? i : 1) - 1];
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: dirsync-linux N\n");
        return 2;
    }
    int n = atoi(argv[1]);
    pid_t kids[2];
    for (int k = 0; k < 2; k++) {
        char root[32];
        snprintf(root, sizeof root, "./removals-%d", k);
        kids[k] = fork();
        if (kids[k] == 0) remover(root, n);
    }
    static uint64_t still[MAX_STRETCHES], still_now[MAX_STRETCHES];
    size_t stretches = 0, unseen = 0, unseen_now = 0;
    uint64_t total = 0, unseen_ns = 0, unseen_now_ns = 0;
    int live = 2, in_d[2] = {0, 0};
    uint64_t began[2] = {0, 0}, disks_first[2] = {0, 0}, disks_last[2] = {0, 0};
    struct counts before[2], last[2], c;
    memset(last, 0, sizeof last);
    while (live > 0) {
        uint64_t disks_now = disks();
        for (int k = 0; k < 2; k++) {
            if (!kids[k]) continue;
            // A child that has exited is reaped here: its task stays readable as a zombie.
            int status;
            if (waitpid(kids[k], &status, WNOHANG) == kids[k]) {
                if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) fprintf(stderr, "remover %d failed\n", k);
                kids[k] = 0;
                live--;
                continue;
            }
            int d = sample(kids[k], &c);
            if (d < 0) continue;
            uint64_t t = now_ns();
            if (d && !in_d[k]) {
                in_d[k] = 1;
                began[k] = t;
                before[k] = last[k];
                disks_first[k] = disks_now;
            }
            if (d) {
                disks_last[k] = disks_now;
            } else if (in_d[k]) {
                in_d[k] = 0;
                uint64_t len = t - began[k];
                if (len >= 1000000) {
                    stretches++;
                    total += len;
                    int grew = c.majflt > before[k].majflt || c.blkio > before[k].blkio ||
                               c.read > before[k].read || c.written > before[k].written ||
                               c.delay > before[k].delay || c.runnable;
                    if (!grew && unseen < MAX_STRETCHES) {
                        still[unseen++] = len;
                        unseen_ns += len;
                        if (disks_last[k] <= disks_first[k]) {
                            still_now[unseen_now++] = len;
                            unseen_now_ns += len;
                        }
                    }
                }
            }
            last[k] = c;
        }
        usleep(SAMPLE_US);
    }
    printf("task_delayacct %llu, sched_schedstats %llu\n",
           (unsigned long long)read_u64("/proc/sys/kernel/task_delayacct", 1),
           (unsigned long long)read_u64("/proc/sys/kernel/sched_schedstats", 1));
    printf("D stretches of 1 ms or more: %zu, %.1f ms\n", stretches, total / 1e6);
    report("shown by no count of the party's own (still before 2026-11-16)", still, unseen, unseen_ns);
    report("of those, with no I/O the host's disks completed within them (still since)", still_now, unseen_now,
           unseen_now_ns);
    return 0;
}

static void report(const char *what, uint64_t *v, size_t n, uint64_t ns) {
    qsort(v, n, sizeof v[0], by_value);
    printf("%s: %.1f ms; ms: n %zu p50 %.2f p90 %.2f p99 %.2f max %.2f\n", what, ns / 1e6, n,
           rank(v, n, 0.5) / 1e6, rank(v, n, 0.9) / 1e6, rank(v, n, 0.99) / 1e6, n ? v[n - 1] / 1e6 : 0.0);
}
