/* Disposable PID1 mechanism probe. Never run on the host or as a workload. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <inttypes.h>
#include <linux/capability.h>
#include <linux/sched.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

/* Existing Shards DOMAIN_FIRST_ID, not a runtime capacity. */
#define MEMBER_UID 200000U
struct observation {
    int error, pidfd, ready, waited, status, second_wait, second_errno;
    unsigned uid, gid;
    uint64_t effective, permitted;
    char cgroup[256];
};
struct held_stat { const char *name; int fd; struct stat before; };

static void poweroff(void) {
    fflush(stdout);
    fflush(stderr);
    sync();
    reboot(RB_POWER_OFF);
    _exit(127);
}
static void fail(const char *step) {
    int saved = errno;
    printf("BIRTH-FAIL {\"step\":\"%s\",\"errno\":%d}\n", step, saved);
    poweroff();
}
static void check(int okay, const char *step) { if (!okay) fail(step); }
static void caps(struct observation *o) {
    struct __user_cap_header_struct h = { .version = _LINUX_CAPABILITY_VERSION_3 };
    struct __user_cap_data_struct d[2] = {{0}};
    if (syscall(SYS_capget, &h, d) != 0) _exit(101);
    o->effective = d[0].effective | ((uint64_t)d[1].effective << 32);
    o->permitted = d[0].permitted | ((uint64_t)d[1].permitted << 32);
    o->uid = getuid(); o->gid = getgid();
}
static void placement(struct observation *o) {
    int fd = open("/proc/self/cgroup", O_RDONLY | O_CLOEXEC);
    if (fd < 0) _exit(102);
    ssize_t n = read(fd, o->cgroup, sizeof(o->cgroup) - 1);
    if (n <= 0 || (size_t)n >= sizeof(o->cgroup) - 1) _exit(103);
    o->cgroup[n] = 0;
    char *nl = strchr(o->cgroup, '\n'); if (nl) *nl = 0;
    close(fd);
}
static struct observation attempt(int directory) {
    struct observation result = { .pidfd = -1 };
    int transfer[2];
    if (pipe2(transfer, O_CLOEXEC) != 0) _exit(104);
    struct clone_args args = {
        .flags = CLONE_PIDFD | CLONE_INTO_CGROUP,
        .pidfd = (uintptr_t)&result.pidfd,
        .exit_signal = SIGCHLD,
        .cgroup = (uint64_t)directory,
    };
    long child = syscall(SYS_clone3, &args, sizeof(args));
    if (child < 0) {
        result.error = errno;
        close(transfer[0]); close(transfer[1]);
        caps(&result); placement(&result);
        return result;
    }
    if (child == 0) {
        close(transfer[0]);
        struct observation first = {0}; caps(&first); placement(&first);
        if (write(transfer[1], &first, sizeof(first)) != (ssize_t)sizeof(first)) _exit(105);
        _exit(0);
    }
    close(transfer[1]);
    struct observation first = {0};
    if (read(transfer[0], &first, sizeof(first)) != (ssize_t)sizeof(first)) _exit(106);
    close(transfer[0]);
    result.uid = first.uid; result.gid = first.gid;
    result.effective = first.effective; result.permitted = first.permitted;
    memcpy(result.cgroup, first.cgroup, sizeof(result.cgroup));
    result.waited = waitpid((pid_t)child, &result.status, 0);
    struct pollfd p = { .fd = result.pidfd, .events = POLLIN };
    result.ready = poll(&p, 1, 0) == 1 && (p.revents & POLLIN);
    result.second_wait = waitpid((pid_t)child, &result.status, WNOHANG);
    result.second_errno = result.second_wait < 0 ? errno : 0;
    close(result.pidfd);
    return result;
}
static void helper(int channel, int job, int sibling, int last_cap) {
    for (int cap = 0; cap <= last_cap; ++cap)
        if (prctl(PR_CAPBSET_DROP, cap, 0, 0, 0) != 0) _exit(107);
    if (setgroups(0, NULL) || setresgid(MEMBER_UID, MEMBER_UID, MEMBER_UID)
        || setresuid(MEMBER_UID, MEMBER_UID, MEMBER_UID)) _exit(108);
    struct __user_cap_header_struct h = { .version = _LINUX_CAPABILITY_VERSION_3 };
    struct __user_cap_data_struct d[2] = {{0}};
    if (syscall(SYS_capset, &h, d) || prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0)
        || prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) _exit(109);
    struct observation initial = {0}; caps(&initial); placement(&initial);
    if (write(channel, &initial, sizeof(initial)) != (ssize_t)sizeof(initial)) _exit(110);
    for (;;) {
        unsigned command;
        if (read(channel, &command, sizeof(command)) != (ssize_t)sizeof(command)) _exit(111);
        if (!command) _exit(0);
        struct observation result = attempt(command == 1 ? job : sibling);
        if (write(channel, &result, sizeof(result)) != (ssize_t)sizeof(result)) _exit(112);
    }
}
static void empty(int directory, const char *step) {
    int fd = openat(directory, "cgroup.events", O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    check(fd >= 0, step);
    char text[256] = {0}; ssize_t n = read(fd, text, sizeof(text) - 1);
    close(fd); check(n > 0 && strstr(text, "populated 0\n"), step);
}
static void run_case(int channel, unsigned command, const char *name, int expected, int job) {
    check(write(channel, &command, sizeof(command)) == (ssize_t)sizeof(command), "send command");
    struct observation o = {0};
    check(read(channel, &o, sizeof(o)) == (ssize_t)sizeof(o), "receive observation");
    printf("BIRTH-CASE {\"name\":\"%s\",\"errno\":%d,\"pidfd\":%d,\"ready\":%d,"
           "\"waited\":%d,\"status\":%d,\"second_wait\":%d,\"second_errno\":%d,"
           "\"uid\":%u,\"gid\":%u,\"effective\":%" PRIu64 ",\"permitted\":%" PRIu64 ",\"placement\":\"%s\"}\n",
           name, o.error, o.pidfd, o.ready, o.waited, o.status, o.second_wait, o.second_errno,
           o.uid, o.gid, o.effective, o.permitted, o.cgroup);
    check(o.error == expected && o.uid == MEMBER_UID && o.gid == MEMBER_UID
          && o.effective == 0 && o.permitted == 0, name);
    if (!expected) check(o.pidfd >= 0 && o.ready && o.waited > 0 && o.status == 0
                         && o.second_wait == -1 && o.second_errno == ECHILD
                         && !strcmp(o.cgroup, "0::/member/job"), name);
    else check(o.pidfd == -1 && !strcmp(o.cgroup, "0::/member"), name);
    empty(job, "job empty after actual helper return");
}
int main(void) {
    /* Refuse before mount/chown unless this is actual guest PID1. */
    if (getpid() != 1 || getuid() != 0) return 126;
    setvbuf(stdout, NULL, _IONBF, 0);
    mkdir("/proc", 0755); mkdir("/cgroup", 0755);
    check(mount("proc", "/proc", "proc", 0, NULL) == 0, "mount proc");
    check(mount("cgroup2", "/cgroup", "cgroup2", 0, NULL) == 0, "mount cgroup2");
    struct utsname u; check(uname(&u) == 0, "uname");
    printf("BIRTH-ENV {\"release\":\"%s\",\"machine\":\"%s\"}\n", u.release, u.machine);
    FILE *f = fopen("/proc/sys/kernel/cap_last_cap", "r");
    int last_cap = -1;
    check(f && fscanf(f, "%d", &last_cap) == 1 && last_cap >= 0, "cap_last_cap");
    fclose(f);
    check(mkdir("/cgroup/member", 0755) == 0, "make member");
    check(mkdir("/cgroup/member/job", 0755) == 0, "make job");
    check(mkdir("/cgroup/sibling", 0755) == 0, "make sibling");
    int member = open("/cgroup/member", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    int job = open("/cgroup/member/job", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    int sibling = open("/cgroup/sibling", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    check(member >= 0 && job >= 0 && sibling >= 0, "held directories");
    int member_procs = openat(member, "cgroup.procs", O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    int job_procs = openat(job, "cgroup.procs", O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    check(member_procs >= 0 && job_procs >= 0, "held procs");
    struct held_stat unchanged[] = {
        {"member directory", member, {0}}, {"job directory", job, {0}},
        {"sibling directory", sibling, {0}},
        {"member subtree", openat(member, "cgroup.subtree_control", O_RDONLY | O_CLOEXEC), {0}},
        {"job subtree", openat(job, "cgroup.subtree_control", O_RDONLY | O_CLOEXEC), {0}},
    };
    for (size_t i=0; i<sizeof(unchanged)/sizeof(*unchanged); ++i)
        check(fstat(unchanged[i].fd, &unchanged[i].before) == 0, "before identity");
    struct stat original_m, original_j;
    check(fstat(member_procs, &original_m) == 0 && fstat(job_procs, &original_j) == 0, "procs identities");
    int channel[2]; check(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, channel) == 0, "channel");
    int helper_pidfd = -1;
    struct clone_args args = { .flags = CLONE_PIDFD | CLONE_INTO_CGROUP,
        .pidfd = (uintptr_t)&helper_pidfd, .exit_signal = SIGCHLD, .cgroup = (uint64_t)member };
    long pid = syscall(SYS_clone3, &args, sizeof(args));
    check(pid >= 0, "outer atomic member birth");
    if (!pid) { close(channel[0]); close(member_procs); close(job_procs); close(member);
        helper(channel[1], job, sibling, last_cap); }
    close(channel[1]);
    struct observation initial = {0};
    check(read(channel[0], &initial, sizeof(initial)) == (ssize_t)sizeof(initial)
          && !strcmp(initial.cgroup, "0::/member") && initial.uid == MEMBER_UID
          && !initial.effective && !initial.permitted, "actual helper principal/source");
    run_case(channel[0], 1, "privileged_opener_only", EACCES, job);
    check(fchown(job_procs, MEMBER_UID, original_j.st_gid) == 0, "destination inode owner");
    run_case(channel[0], 1, "destination_only", EACCES, job);
    check(fchown(member_procs, MEMBER_UID, original_m.st_gid) == 0, "common ancestor inode owner");
    run_case(channel[0], 1, "destination_and_common_ancestor", 0, job);
    run_case(channel[0], 2, "unrelated_destination", EACCES, job);
    check(fchown(member_procs, original_m.st_uid, original_m.st_gid) == 0, "restore ancestor");
    run_case(channel[0], 1, "ancestor_restored_denial", EACCES, job);
    check(fchown(job_procs, original_j.st_uid, original_j.st_gid) == 0, "restore destination");
    unsigned stop = 0; check(write(channel[0], &stop, sizeof(stop)) == (ssize_t)sizeof(stop), "stop helper");
    close(channel[0]); int status;
    check(waitpid((pid_t)pid, &status, 0) == pid && status == 0, "actual helper reap");
    struct pollfd polled = { .fd = helper_pidfd, .events = POLLIN };
    check(poll(&polled, 1, 0) == 1 && (polled.revents & POLLIN), "helper pidfd exit");
    close(helper_pidfd);
    empty(member, "actual member empty");
    for (size_t i=0; i<sizeof(unchanged)/sizeof(*unchanged); ++i) {
        struct stat after; check(fstat(unchanged[i].fd, &after) == 0, "after identity");
        check(after.st_uid == unchanged[i].before.st_uid && after.st_gid == unchanged[i].before.st_gid
              && after.st_mode == unchanged[i].before.st_mode && after.st_ino == unchanged[i].before.st_ino
              && after.st_dev == unchanged[i].before.st_dev, unchanged[i].name);
        if (i >= 3) close(unchanged[i].fd);
    }
    struct stat restored;
    check(fstat(member_procs, &restored) == 0 && restored.st_uid == original_m.st_uid
          && restored.st_gid == original_m.st_gid && restored.st_mode == original_m.st_mode, "restored member inode");
    check(fstat(job_procs, &restored) == 0 && restored.st_uid == original_j.st_uid
          && restored.st_gid == original_j.st_gid && restored.st_mode == original_j.st_mode, "restored job inode");
    close(member_procs); close(job_procs); close(job); close(sibling); close(member);
    check(rmdir("/cgroup/member/job") == 0 && rmdir("/cgroup/member") == 0
          && rmdir("/cgroup/sibling") == 0, "remove exact empty scopes");
    puts("BIRTH-PASS {\"cases\":5,\"helper_reaped\":true,\"member_populated\":0,\"directories_controllers_unchanged\":true,\"procs_restored\":true,\"actor_isolation\":\"Unverified\"}");
    poweroff();
}
