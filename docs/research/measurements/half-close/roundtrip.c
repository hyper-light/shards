/*
 * M236 (docs/research/platform-measurements.md): what a read that waits for a message
 * costs, waiting in recv(2) and in the ways a read can wait instead. Two processes pass
 * one byte back and forth over an AF_UNIX socketpair, each read waiting for it, each side
 * spinning PAUSE microseconds before it sends, so that with a PAUSE the other's wait
 * begins before the byte comes, as a reader's that waits on another process's work does.
 * Each read waits:
 *
 *   recv    in a recv(2) that may block;
 *   poll    in poll(2), after a recv(2) with MSG_DONTWAIT that found nothing, then reads
 *           with MSG_DONTWAIT again;
 *   checked as poll, with fcntl(F_GETFL) and getsockopt(SO_RCVTIMEO) before the poll(2),
 *           as a read that keeps its descriptor's O_NONBLOCK and receive timeout must;
 *   kevent  as poll, in kevent(2) on a kqueue the reader keeps, the socket added once
 *           (macOS and the BSDs);
 *   oneshot as kevent, the socket added to the kqueue with EV_ONESHOT in the kevent(2)
 *           that waits, as one kqueue a thread keeps for any socket would be.
 *
 *   roundtrip BATCHES ROUNDS PAUSE
 *
 * BATCHES batches of ROUNDS round trips each way, the ways in turn, their order rotated
 * each batch, so that all see the same host. Each round trip is timed on the requester's
 * side, the echoer's pause taken off, the first of each turn left out (the echoer's read
 * then began the other way). Reports n, p50, p90, p99 and max of each, in microseconds.
 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#if defined(__APPLE__) || defined(__FreeBSD__)
#include <sys/event.h>
#define HAVE_KEVENT 1
#endif

enum { RECV, POLL, CHECKED, KEVENT, ONESHOT, WAYS };
static const char *names[WAYS] = {"recv", "poll", "checked", "kevent", "oneshot"};

static uint64_t now_ns(void) {
#ifdef __APPLE__
    /* CLOCK_MONOTONIC's clock_gettime(2) gives microseconds on macOS. */
    return clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
#else
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
#endif
}

static uint64_t pause_ns;

/* Spins `pause_ns`: a sleep's wake-up would swamp what is measured. */
static void spin(void) {
    uint64_t until = now_ns() + pause_ns;
    while (now_ns() < until) {
    }
}

/* The kqueue a process waits on `fd` in, `fd` added to it once; and one it adds `fd` to
 * as it waits. */
static int kq = -1, kq_bare = -1;

/* One byte from `fd`, waited for the way `way` says. */
static ssize_t take(int fd, char *b, int way) {
    if (way == RECV)
        return recv(fd, b, 1, 0);
    for (;;) {
        ssize_t n = recv(fd, b, 1, MSG_DONTWAIT);
        if (n >= 0 || (errno != EAGAIN && errno != EINTR))
            return n;
        if (errno == EINTR)
            continue;
        if (way == CHECKED) {
            struct timeval tv;
            socklen_t len = sizeof tv;
            if (fcntl(fd, F_GETFL) < 0 || getsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, &len) < 0)
                return -1;
        }
#ifdef HAVE_KEVENT
        if (way == KEVENT) {
            struct kevent ev;
            if (kevent(kq, NULL, 0, &ev, 1, NULL) < 0 && errno != EINTR)
                return -1;
            continue;
        }
        if (way == ONESHOT) {
            struct kevent add, ev;
            EV_SET(&add, fd, EVFILT_READ, EV_ADD | EV_ONESHOT, 0, 0, NULL);
            if (kevent(kq_bare, &add, 1, &ev, 1, NULL) < 0 && errno != EINTR)
                return -1;
            continue;
        }
#endif
        struct pollfd p = {.fd = fd, .events = POLLIN};
        if (poll(&p, 1, -1) < 0 && errno != EINTR)
            return -1;
    }
}

static void keep_kqueue(int fd) {
#ifdef HAVE_KEVENT
    struct kevent add;
    kq = kqueue();
    kq_bare = kqueue();
    EV_SET(&add, fd, EVFILT_READ, EV_ADD | EV_CLEAR, 0, 0, NULL);
    if (kq < 0 || kq_bare < 0 || kevent(kq, &add, 1, NULL, 0, NULL) < 0) {
        perror("kqueue");
        exit(1);
    }
#else
    (void)fd;
#endif
}

static int cmp(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static void report(const char *name, uint64_t *v, size_t n) {
    qsort(v, n, sizeof *v, cmp);
    printf("%s: n %zu, p50 %.2f, p90 %.2f, p99 %.2f, max %.2f us\n", name, n, (double)v[n / 2] / 1e3,
           (double)v[n * 9 / 10] / 1e3, (double)v[n * 99 / 100] / 1e3, (double)v[n - 1] / 1e3);
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: roundtrip BATCHES ROUNDS PAUSE\n");
        return 2;
    }
    int batches = atoi(argv[1]), rounds = atoi(argv[2]);
    pause_ns = (uint64_t)atoi(argv[3]) * 1000u;
#ifdef HAVE_KEVENT
    int ways = WAYS;
#else
    int ways = KEVENT;
#endif
    int sv[2];
    if (batches < 1 || rounds < 2 || socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) {
        perror("roundtrip");
        return 1;
    }
    pid_t echoer = fork();
    if (echoer == 0) {
        /* Echoes each byte; a byte says how its sender waits, and so how this waits next. */
        close(sv[0]);
        keep_kqueue(sv[1]);
        char b = RECV;
        while (take(sv[1], &b, b) == 1) {
            spin();
            if (send(sv[1], &b, 1, 0) != 1)
                break;
        }
        _exit(0);
    }
    close(sv[1]);
    keep_kqueue(sv[0]);
    size_t each = (size_t)batches * (size_t)(rounds - 1);
    uint64_t *took[WAYS];
    size_t have[WAYS] = {0};
    for (int w = 0; w < WAYS; w++)
        took[w] = calloc(each, sizeof(uint64_t));
    for (int batch = 0; batch < batches; batch++) {
        for (int turn = 0; turn < ways; turn++) {
            int way = (batch + turn) % ways;
            char b = (char)way;
            for (int round = 0; round < rounds; round++) {
                spin();
                uint64_t start = now_ns();
                if (send(sv[0], &b, 1, 0) != 1 || take(sv[0], &b, way) != 1) {
                    perror("round trip");
                    return 1;
                }
                if (round > 0)
                    took[way][have[way]++] = now_ns() - start - pause_ns;
            }
        }
    }
    close(sv[0]);
    waitpid(echoer, NULL, 0);
    for (int w = 0; w < ways; w++)
        report(names[w], took[w], have[w]);
    return 0;
}
