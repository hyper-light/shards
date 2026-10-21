// M253 (docs/research/platform-measurements.md): how each end of a FIFO learns the other
// has closed (D185), on macOS and on Linux.
//
// Part 1, n rounds of each: a child opens the FIFO's other end, writes (or reads) a byte,
// waits 200 ms and exits; the parent waits on its own end with poll(2), select(2), and
// kqueue(2) (macOS) or epoll(7) (Linux), and says whether it woke, and when, from the
// child's close, then what its next read or write found.
//
// Part 2: does a blocking read(2), select(2) or poll(2) ever sleep through the writer's
// close? The writer closes at a random point from 0 to 2 ms after the reader has begun to
// wait; a wait not over 1.1 s later is stuck (SIGALRM, without SA_RESTART, ends it).
//
//   ends DIR ROUNDS TRIALS
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#ifdef __APPLE__
#include <sys/event.h>
#define QUEUE "kqueue"
#else
#include <sys/epoll.h>
#define QUEUE "epoll"
#endif

static char path[512];

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

// The other end, in a child: opened, a byte written or read, 200 ms, closed; the close's
// time sent on `report`.
static pid_t other_end(int writes, int report) {
    pid_t pid = fork();
    if (pid == 0) {
        int fd = open(path, writes ? O_WRONLY : O_RDONLY);
        if (fd < 0) _exit(10);
        char c = 'x';
        if (writes ? write(fd, &c, 1) != 1 : read(fd, &c, 1) != 1) _exit(11);
        usleep(200000);
        double t = now();
        close(fd);
        if (write(report, &t, sizeof t) != sizeof t) _exit(12);
        _exit(0);
    }
    return pid;
}

// Waits up to 3 s on `fd` for `events` (POLLIN or 0) with `how`; says what woke it in `said`.
static int wait_on(int fd, const char *how, int reading, char *said, size_t len) {
    int n = 0;
    if (!strcmp(how, "poll")) {
        struct pollfd p = {fd, reading ? POLLIN : 0, 0};
        n = poll(&p, 1, 3000);
        snprintf(said, len, "%s%s%s%s", p.revents & POLLIN ? " POLLIN" : "",
                 p.revents & POLLHUP ? " POLLHUP" : "", p.revents & POLLERR ? " POLLERR" : "",
                 p.revents & POLLNVAL ? " POLLNVAL" : "");
    } else if (!strcmp(how, "select")) {
        fd_set s;
        FD_ZERO(&s);
        FD_SET(fd, &s);
        struct timeval tv = {3, 0};
        n = reading ? select(fd + 1, &s, NULL, NULL, &tv) : select(fd + 1, NULL, NULL, &s, &tv);
        snprintf(said, len, "%s", n > 0 ? (reading ? " readable" : " exceptional") : "");
    } else {
#ifdef __APPLE__
        int kq = kqueue();
        struct kevent ev;
        EV_SET(&ev, fd, reading ? EVFILT_READ : EVFILT_WRITE, EV_ADD | EV_CLEAR, 0, 0, NULL);
        if (kevent(kq, &ev, 1, NULL, 0, NULL) < 0) {
            snprintf(said, len, " kevent: %s", strerror(errno));
            close(kq);
            return -1;
        }
        struct timespec ts = {3, 0};
        struct kevent got;
        n = kevent(kq, NULL, 0, &got, 1, &ts);
        // A writer's first event is the room it has now: the next is what changed.
        if (!reading && n > 0 && !(got.flags & EV_EOF)) n = kevent(kq, NULL, 0, &got, 1, &ts);
        snprintf(said, len, "%s", n > 0 && (got.flags & EV_EOF) ? " EV_EOF" : n > 0 ? " no EV_EOF" : "");
        close(kq);
#else
        int ep = epoll_create1(0);
        struct epoll_event ev = {.events = reading ? EPOLLIN : 0, .data.fd = fd};
        if (epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ev) < 0) {
            snprintf(said, len, " epoll_ctl: %s", strerror(errno));
            close(ep);
            return -1;
        }
        struct epoll_event got;
        n = epoll_wait(ep, &got, 1, 3000);
        snprintf(said, len, "%s%s%s", n > 0 && (got.events & EPOLLIN) ? " EPOLLIN" : "",
                 n > 0 && (got.events & EPOLLHUP) ? " EPOLLHUP" : "",
                 n > 0 && (got.events & EPOLLERR) ? " EPOLLERR" : "");
        close(ep);
#endif
    }
    return n;
}

static void reader(const char *how) {
    int report[2];
    if (pipe(report)) return;
    int fd = open(path, O_RDONLY | O_NONBLOCK);
    pid_t pid = other_end(1, report[1]);
    char c;
    while (read(fd, &c, 1) != 1) usleep(1000);
    char said[128] = "";
    int n = wait_on(fd, how, 1, said, sizeof said);
    double woke = now();
    ssize_t got = read(fd, &c, 1);
    double closed = 0;
    if (read(report[0], &closed, sizeof closed) != sizeof closed) closed = 0;
    waitpid(pid, NULL, 0);
    if (n > 0)
        printf("reader  %-6s woke%s, %+.1f ms from the writer's close; read then %zd\n", how, said,
               (woke - closed) * 1e3, got);
    else
        printf("reader  %-6s no wake in 3 s%s; read then %zd\n", how, said, got);
    close(fd);
    close(report[0]);
    close(report[1]);
}

static void writer(const char *how) {
    int report[2];
    if (pipe(report)) return;
    pid_t pid = other_end(0, report[1]);
    int fd;
    // A non-blocking open for writing fails (ENXIO) until a reader has it open.
    while ((fd = open(path, O_WRONLY | O_NONBLOCK)) < 0) usleep(1000);
    char c = 'y';
    if (write(fd, &c, 1) != 1) perror("write");
    char said[128] = "";
    int n = wait_on(fd, how, 0, said, sizeof said);
    double woke = now();
    double closed = 0;
    if (read(report[0], &closed, sizeof closed) != sizeof closed) closed = 0;
    waitpid(pid, NULL, 0);
    ssize_t put = write(fd, &c, 1);
    int e = errno;
    if (n > 0)
        printf("writer  %-6s woke%s, %+.1f ms from the reader's close; write then %zd%s%s\n", how,
               said, (woke - closed) * 1e3, put, put < 0 ? " " : "", put < 0 ? strerror(e) : "");
    else
        printf("writer  %-6s no wake in 3 s%s; write then %zd%s%s\n", how, said, put,
               put < 0 ? " " : "", put < 0 ? strerror(e) : "");
    close(fd);
    close(report[0]);
    close(report[1]);
}

static void ring(int s) { (void)s; }

static void trials(const char *how, int n) {
    int stuck = 0, ended = 0, other = 0;
    srand(7);
    for (int i = 0; i < n; i++) {
        unlink(path);
        if (mkfifo(path, 0600)) {
            perror("mkfifo");
            return;
        }
        int go[2];
        if (pipe(go)) return;
        int delay = rand() % 2000;
        pid_t pid = fork();
        if (pid == 0) {
            int w = open(path, O_WRONLY);
            char c;
            if (read(go[0], &c, 1) != 1) _exit(3);
            usleep(delay);
            close(w);
            _exit(0);
        }
        int r = open(path, O_RDONLY);
        char c = 'g';
        if (write(go[1], &c, 1) != 1) return;
        struct itimerval t = {{0, 0}, {1, 100000}};
        setitimer(ITIMER_REAL, &t, NULL);
        ssize_t got = -1;
        if (!strcmp(how, "read")) {
            got = read(r, &c, 1);
        } else if (!strcmp(how, "select")) {
            fd_set s;
            FD_ZERO(&s);
            FD_SET(r, &s);
            if (select(r + 1, &s, NULL, NULL, NULL) > 0) got = read(r, &c, 1);
        } else {
            struct pollfd p = {r, POLLIN, 0};
            if (poll(&p, 1, -1) > 0) got = read(r, &c, 1);
        }
        int e = errno;
        struct itimerval off = {{0, 0}, {0, 0}};
        setitimer(ITIMER_REAL, &off, NULL);
        if (got == 0) ended++;
        else if (got < 0 && e == EINTR) stuck++;
        else other++;
        waitpid(pid, NULL, 0);
        close(r);
        close(go[0]);
        close(go[1]);
    }
    unlink(path);
    printf("trials  %-6s n=%d: the writer's close seen %d, stuck past 1.1 s %d, other %d\n", how, n,
           ended, stuck, other);
}

int main(int argc, char **argv) {
    if (argc < 4) {
        fprintf(stderr, "usage: ends DIR ROUNDS TRIALS\n");
        return 2;
    }
    snprintf(path, sizeof path, "%s/fifo", argv[1]);
    int rounds = atoi(argv[2]), n = atoi(argv[3]);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = ring;
    sigaction(SIGALRM, &sa, NULL);
    signal(SIGPIPE, SIG_IGN);
    unlink(path);
    if (mkfifo(path, 0600)) {
        perror("mkfifo");
        return 1;
    }
    const char *hows[] = {"poll", "select", QUEUE};
    for (int r = 0; r < rounds; r++)
        for (int i = 0; i < 3; i++) reader(hows[i]);
    for (int r = 0; r < rounds; r++)
        for (int i = 0; i < 3; i++) writer(hows[i]);
    unlink(path);
    trials("read", n);
    trials("select", n);
    // A wait that misses the close costs the whole 1.1 s: a few trials say so.
    trials("poll", n < 20 ? n : 20);
    return 0;
}
