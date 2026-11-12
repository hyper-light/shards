// softvirt-memlayout: how many host mappings a guest's memory would need under a
// software hypervisor that maps guest pages into host processes (one host process per
// guest address space, guest-physical memory a file): one mapping per run of pages whose
// virtual addresses and guest-physical frames both follow on. Run as root in a guest
// (pagemap gives frame numbers to CAP_SYS_ADMIN only); it walks every process's
// /proc/PID/maps and /proc/PID/pagemap and prints, per process and in total: guest
// mappings (VMAs), resident pages, and runs, at 4 KiB and within 2 MiB-aligned units.
// Method and results: docs/research/platform-measurements.md (the softvirt entries).
//
//   cc -O2 -static -o pagemap pagemap.c && ./pagemap [-v]

#define _GNU_SOURCE
#include <ctype.h>
#include <dirent.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define PAGE 4096ULL
#define PFN_MASK ((1ULL << 55) - 1)
#define PRESENT (1ULL << 63)

struct tally {
    uint64_t vmas, resident, runs, huge_runs;
};

// Walks one process: a run ends where the next page is absent, not the next virtual
// page, or not the next frame; huge_runs counts runs that a 2 MiB-aligned unit splits.
static int walk(const char *pid, struct tally *t) {
    char path[64];
    snprintf(path, sizeof path, "/proc/%s/maps", pid);
    FILE *maps = fopen(path, "r");
    if (!maps)
        return -1;
    snprintf(path, sizeof path, "/proc/%s/pagemap", pid);
    int pm = open(path, O_RDONLY);
    if (pm < 0) {
        fclose(maps);
        return -1;
    }
    char line[512];
    static uint64_t buf[512];
    while (fgets(line, sizeof line, maps)) {
        uint64_t start, end;
        if (sscanf(line, "%" SCNx64 "-%" SCNx64, &start, &end) != 2)
            continue;
        if (strstr(line, "[vsyscall]"))
            continue;
        t->vmas++;
        uint64_t prev_pfn = 0, prev_va = 0;
        int in_run = 0;
        for (uint64_t va = start; va < end;) {
            uint64_t want = (end - va) / PAGE;
            if (want > 512)
                want = 512;
            ssize_t got = pread(pm, buf, want * 8, (off_t)(va / PAGE * 8));
            if (got <= 0)
                break;
            for (uint64_t i = 0; i < (uint64_t)got / 8; i++, va += PAGE) {
                uint64_t e = buf[i];
                if (!(e & PRESENT) || (e & PFN_MASK) == 0) {
                    in_run = 0;
                    continue;
                }
                uint64_t pfn = e & PFN_MASK;
                t->resident++;
                int follows = in_run && va == prev_va + PAGE && pfn == prev_pfn + 1;
                if (!follows)
                    t->runs++;
                if (!follows || va % (2ULL << 20) == 0)
                    t->huge_runs++;
                in_run = 1;
                prev_va = va;
                prev_pfn = pfn;
            }
        }
    }
    close(pm);
    fclose(maps);
    return 0;
}

int main(int argc, char **argv) {
    int verbose = argc > 1 && !strcmp(argv[1], "-v");
    DIR *d = opendir("/proc");
    if (!d)
        return 2;
    struct tally all = {0};
    size_t procs = 0, unreadable = 0;
    struct dirent *e;
    if (verbose)
        printf("| pid | command | guest mappings | resident pages | runs | runs within 2 MiB units |\n|---|---|---|---|---|---|\n");
    while ((e = readdir(d))) {
        if (!isdigit((unsigned char)e->d_name[0]))
            continue;
        struct tally t = {0};
        if (walk(e->d_name, &t)) {
            unreadable++;
            continue;
        }
        if (t.resident == 0)
            continue; // kernel threads
        procs++;
        all.vmas += t.vmas;
        all.resident += t.resident;
        all.runs += t.runs;
        all.huge_runs += t.huge_runs;
        if (verbose) {
            char path[64], comm[64] = {0};
            snprintf(path, sizeof path, "/proc/%s/comm", e->d_name);
            FILE *f = fopen(path, "r");
            if (f) {
                if (!fgets(comm, sizeof comm, f))
                    comm[0] = 0;
                fclose(f);
            }
            comm[strcspn(comm, "\n")] = 0;
            printf("| %s | %s | %" PRIu64 " | %" PRIu64 " | %" PRIu64 " | %" PRIu64 " |\n", e->d_name,
                   comm, t.vmas, t.resident, t.runs, t.huge_runs);
        }
    }
    closedir(d);
    printf("- processes with resident pages: %zu (unreadable: %zu)\n", procs, unreadable);
    printf("- guest mappings (VMAs): %" PRIu64 "\n", all.vmas);
    printf("- resident pages: %" PRIu64 " (%" PRIu64 " MiB)\n", all.resident, (uint64_t)(all.resident * PAGE >> 20));
    printf("- runs (host mappings at 4 KiB): %" PRIu64 ", %.1f pages a run\n", all.runs,
           all.runs ? (double)all.resident / (double)all.runs : 0.0);
    printf("- runs split at 2 MiB units: %" PRIu64 "\n", all.huge_runs);
    return 0;
}
