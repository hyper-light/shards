/*
 * probe: does the kernel's AF_UNIX garbage collector flush a socket that is in flight
 * (SCM_RIGHTS) once its sender has closed its own descriptor for it?
 *
 * One trial, as shards' daemon hands a client's connection to a warm VM:
 *   socketpair(h)  the handoff channel; h[1] is held by the receiver, never in flight
 *   socketpair(c)  the passed connection; c[0] stays with the "client", c[1] is passed
 *   sendmsg(h[0], 1 byte, SCM_RIGHTS {c[1]})
 *   close(c[1]) before the receive ("closed") or after it ("held")
 *   with "trigger": free an unrelated Unix socket, which on XNU schedules unp_gc
 *   usleep(delay), recvmsg(h[1]) -> r
 *   write(c[0], 1 byte); recv(r, MSG_DONTWAIT)
 * A trial is "flushed" when that recv sees end-of-stream: the collector ran sorflush on
 * the socket while it was in flight.
 *
 * cc -O2 -o probe probe.c && ./probe [trials]
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/utsname.h>
#include <unistd.h>

static int pass(int via, int fd) {
    char byte = 'x';
    struct iovec iov = {.iov_base = &byte, .iov_len = 1};
    union {
        struct cmsghdr h;
        char buf[CMSG_SPACE(sizeof(int))];
    } u;
    memset(&u, 0, sizeof u);
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.buf,
                         .msg_controllen = CMSG_SPACE(sizeof(int))};
    struct cmsghdr *cm = CMSG_FIRSTHDR(&msg);
    cm->cmsg_level = SOL_SOCKET;
    cm->cmsg_type = SCM_RIGHTS;
    cm->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cm), &fd, sizeof(int));
    return sendmsg(via, &msg, 0) == 1 ? 0 : -1;
}

static int take(int via) {
    char byte;
    struct iovec iov = {.iov_base = &byte, .iov_len = 1};
    union {
        struct cmsghdr h;
        char buf[CMSG_SPACE(sizeof(int))];
    } u;
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.buf,
                         .msg_controllen = sizeof u.buf};
    if (recvmsg(via, &msg, 0) != 1) return -1;
    struct cmsghdr *cm = CMSG_FIRSTHDR(&msg);
    if (!cm || cm->cmsg_type != SCM_RIGHTS) return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(cm), sizeof(int));
    return fd;
}

/* 1: intact, 0: flushed (end-of-stream), -1: error */
static int trial(int held, int trigger, int delay_us) {
    int h[2], c[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, h) || socketpair(AF_UNIX, SOCK_STREAM, 0, c)) return -1;
    if (pass(h[0], c[1])) return -1;
    if (!held) close(c[1]);
    if (trigger) {
        int t[2];
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, t)) return -1;
        close(t[0]);
        close(t[1]);
    }
    if (delay_us) usleep(delay_us);
    int r = take(h[1]);
    if (held) close(c[1]);
    if (r < 0) return -1;
    if (write(c[0], "p", 1) != 1) return -1;
    char got;
    ssize_t n = recv(r, &got, 1, MSG_DONTWAIT);
    int result = n == 1 ? 1 : n == 0 ? 0 : -1;
    if (result < 0) fprintf(stderr, "recv: %s\n", strerror(errno));
    close(r); close(c[0]); close(h[0]); close(h[1]);
    return result;
}

int main(int argc, char **argv) {
    int trials = argc > 1 ? atoi(argv[1]) : 1000;
    struct utsname u;
    uname(&u);
    printf("host: %s %s %s %s\n", u.sysname, u.release, u.version, u.machine);
    printf("%-7s %-8s %8s %8s %8s %8s\n", "sender", "trigger", "delay_us", "trials", "flushed", "errors");
    const int delays[] = {0, 100, 1000};
    for (int held = 0; held <= 1; held++)
        for (int trigger = 0; trigger <= 1; trigger++)
            for (unsigned d = 0; d < sizeof delays / sizeof *delays; d++) {
                int flushed = 0, errors = 0;
                for (int i = 0; i < trials; i++) {
                    int r = trial(held, trigger, delays[d]);
                    flushed += r == 0;
                    errors += r < 0;
                }
                printf("%-7s %-8s %8d %8d %8d %8d\n", held ? "held" : "closed",
                       trigger ? "yes" : "no", delays[d], trials, flushed, errors);
                fflush(stdout);
            }
    return 0;
}
