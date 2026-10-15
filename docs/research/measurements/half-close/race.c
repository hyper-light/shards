/*
 * M236 (docs/research/platform-measurements.md): a read waiting on a stream socket, and
 * its peer's shutdown(SHUT_WR) landing as the read starts. How many readers are still
 * asleep a second after the peer ended its side? And what does shutdown(SHUT_RDWR) do on
 * a socket whose peer has already ended its side?
 *
 *   race unix-recv N SPREAD        recv(2) that may block, on an AF_UNIX socketpair
 *   race unix-poll N SPREAD        poll(2) for POLLIN, then recv(2) with MSG_DONTWAIT
 *   race tcp-recv N SPREAD DATA    recv(2) that may block, on TCP over loopback; with
 *                                  DATA 1, after reading what the peer sent first
 *   race rdwr                      shutdown(SHUT_RDWR) after the peer's SHUT_WR, and
 *                                  with the peer still open, on AF_UNIX and on TCP
 *
 * Each round, a reader and the peer wait on one flag, then spin a random 0..SPREAD
 * iterations each, so that the half-close lands at points spread around the reader's
 * entry into the kernel. A reader still waiting a second after the peer's shutdown
 * returned counts as asleep; a poll(2) of its socket at that moment says whether its end
 * was there to read. Its sockets are left as they are.
 */
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

enum mode { UNIX_RECV, UNIX_POLL, TCP_RECV };

static enum mode mode;
static int reader_fd, peer_fd, with_data;
static atomic_int go;
static int reader_spins, peer_spins;
static char buf[65536];

static ssize_t polled_recv(int fd, char *b, size_t n) {
    for (;;) {
        struct pollfd p = {.fd = fd, .events = POLLIN};
        if (poll(&p, 1, -1) < 0 && errno != EINTR)
            return -1;
        ssize_t got = recv(fd, b, n, MSG_DONTWAIT);
        if (got >= 0 || (errno != EAGAIN && errno != EINTR))
            return got;
    }
}

static void *reader(void *out) {
    static __thread char b[65536];
    if (with_data)
        (void)recv(reader_fd, b, sizeof b, 0);
    while (!atomic_load(&go)) {
    }
    for (volatile int i = 0; i < reader_spins; i++) {
    }
    *(volatile ssize_t *)out =
        mode == UNIX_POLL ? polled_recv(reader_fd, b, sizeof b) : recv(reader_fd, b, sizeof b, 0);
    return NULL;
}

static void *peer(void *unused) {
    (void)unused;
    while (!atomic_load(&go)) {
    }
    for (volatile int i = 0; i < peer_spins; i++) {
    }
    shutdown(peer_fd, SHUT_WR);
    return NULL;
}

static int tcp_listener(struct sockaddr_in *at) {
    int l = socket(AF_INET, SOCK_STREAM, 0), one = 1;
    socklen_t len = sizeof *at;
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    memset(at, 0, sizeof *at);
    at->sin_family = AF_INET;
    at->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(l, (struct sockaddr *)at, sizeof *at) || listen(l, 64) ||
        getsockname(l, (struct sockaddr *)at, &len)) {
        perror("listener");
        exit(1);
    }
    return l;
}

/* A connected pair: [0] reads, [1] is its peer. TCP pairs reset as they close, so that
 * no round leaves a TIME_WAIT behind to use up the ephemeral ports. */
static int pair(int l, const struct sockaddr_in *at, int fds[2]) {
    if (mode != TCP_RECV)
        return socketpair(AF_UNIX, SOCK_STREAM, 0, fds);
    struct linger reset = {1, 0};
    fds[1] = socket(AF_INET, SOCK_STREAM, 0);
    if (connect(fds[1], (const struct sockaddr *)at, sizeof *at))
        return -1;
    fds[0] = accept(l, NULL, NULL);
    setsockopt(fds[0], SOL_SOCKET, SO_LINGER, &reset, sizeof reset);
    setsockopt(fds[1], SOL_SOCKET, SO_LINGER, &reset, sizeof reset);
    return fds[0] < 0 ? -1 : 0;
}

static int race(int rounds, int spread) {
    struct sockaddr_in at;
    int l = mode == TCP_RECV ? tcp_listener(&at) : -1;
    int asleep = 0, ended_there = 0;
    srand(1);
    for (int i = 0; i < rounds; i++) {
        int fds[2];
        if (pair(l, &at, fds)) {
            perror("pair");
            return 1;
        }
        reader_fd = fds[0];
        peer_fd = fds[1];
        if (with_data)
            send(peer_fd, buf, sizeof buf, 0);
        volatile ssize_t got = -2;
        pthread_t r, p;
        atomic_store(&go, 0);
        reader_spins = rand() % spread;
        peer_spins = rand() % spread;
        pthread_create(&r, NULL, reader, (void *)&got);
        pthread_create(&p, NULL, peer, NULL);
        usleep(50);
        atomic_store(&go, 1);
        pthread_join(p, NULL);
        for (int waited = 0; got == -2 && waited < 1000; waited++)
            usleep(1000);
        if (got == -2) {
            struct pollfd poll_fd = {.fd = reader_fd, .events = POLLIN};
            asleep++;
            ended_there += poll(&poll_fd, 1, 0) > 0;
            pthread_detach(r);
            continue;
        }
        pthread_join(r, NULL);
        close(fds[0]);
        close(fds[1]);
    }
    printf("%s: %d rounds, spread %d%s: %d readers asleep a second after the half-close, "
           "%d of them on a socket poll(2) found ended\n",
           mode == UNIX_RECV ? "unix-recv" : mode == UNIX_POLL ? "unix-poll" : "tcp-recv",
           rounds, spread, with_data ? ", data first" : "", asleep, ended_there);
    return 0;
}

static const char *shut_rdwr(int fds[2], int peer_first) {
    static char said[160];
    if (peer_first) {
        shutdown(fds[1], SHUT_WR);
        usleep(100000);
    }
    int rc = shutdown(fds[0], SHUT_RDWR);
    int err = errno;
    struct pollfd p = {.fd = fds[1], .events = POLLIN};
    int ended = poll(&p, 1, 1000) > 0 && recv(fds[1], buf, 1, MSG_DONTWAIT) == 0;
    snprintf(said, sizeof said, "shutdown(SHUT_RDWR) %s; the peer %s its end", rc ? strerror(err) : "succeeds",
             ended ? "reads" : "never reads");
    return said;
}

static int rdwr(void) {
    struct sockaddr_in at;
    int l = tcp_listener(&at);
    for (int tcp = 0; tcp < 2; tcp++) {
        for (int peer_first = 0; peer_first < 2; peer_first++) {
            int fds[2];
            mode = tcp ? TCP_RECV : UNIX_RECV;
            if (pair(l, &at, fds)) {
                perror("pair");
                return 1;
            }
            printf("rdwr %s, peer %s: %s\n", tcp ? "tcp" : "unix",
                   peer_first ? "half-closed first" : "open", shut_rdwr(fds, peer_first));
            close(fds[0]);
            close(fds[1]);
        }
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "rdwr"))
        return rdwr();
    if (argc < 4) {
        fprintf(stderr, "usage: race unix-recv|unix-poll|tcp-recv ROUNDS SPREAD [DATA] | race rdwr\n");
        return 2;
    }
    mode = !strcmp(argv[1], "unix-poll") ? UNIX_POLL : !strcmp(argv[1], "tcp-recv") ? TCP_RECV : UNIX_RECV;
    with_data = argc > 4 && mode == TCP_RECV && atoi(argv[4]);
    return race(atoi(argv[2]), atoi(argv[3]) > 0 ? atoi(argv[3]) : 1);
}
