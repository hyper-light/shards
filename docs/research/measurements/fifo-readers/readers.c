// Many readers of one FIFO, as a home's clients wait on its list of request slots (D185,
// PM M262): does a reader that waits for the next record ever sleep through records the
// others take, and which ways of waiting see every one?
//
//   cc -O2 -pthread -o readers readers.c && ./readers DIR CLIENTS SECONDS MODE [PAUSE_US] [threads|processes]
//
// A maker keeps two 16-byte records waiting on the FIFO DIR/list, counted as the bytes its
// own reader has to read (FIONREAD), and writes another each time a client says it took
// one, as shards' slots' maker does. Each of CLIENTS threads or processes, over and over, takes one
// record as a client of shards does (slots::take): opens the list not waiting and reads;
// where nothing is there, it waits as MODE says, then reads again, until it has one:
//
//   select-block  select(2), then read(2) with the descriptor set to wait (shards on macOS)
//   read          read(2) with the descriptor set to wait, nothing before (shards on Linux)
//   select        select(2), then read(2) not waiting; on EAGAIN, select(2) again
//   poll          poll(2), then as `select`
//   kqueue        kevent(2) for EVFILT_READ, then as `select` (macOS)
//   epoll         epoll_wait(2) for EPOLLIN, then as `select` (Linux)
//
// It then closes it, says it took one, and pauses PAUSE_US (0 by default), as a client's
// request and the next client's launch take. A watchdog says which client has waited 5 s
// while the others took 100 records or more, where (waiting, or in its read), and how often
// its wait returned to it meanwhile.
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <sys/stat.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#ifdef __APPLE__
#include <sys/event.h>
#endif
#ifdef __linux__
#include <sys/epoll.h>
#endif

#define RECORD 16
#define MAX_CLIENTS 64

enum mode { SELECT_BLOCK, READ, SELECT, POLL, KQUEUE, EPOLL };
static const char *names[] = {"select-block", "read", "select", "poll", "kqueue", "epoll"};

static char path[1024];
static int told[2];
static enum mode mode;
static long pause_us;

struct client {
    atomic_int where;        // 0 between takes, 1 waiting, 2 in its read
    atomic_long since_ns;    // when its wait began
    atomic_long taken_then;  // `taken` then
    atomic_long woke;        // how often its wait returned in this one
    long stuck_reported;
};
struct shared {
    atomic_int stop;
    atomic_long taken;
    struct client clients[MAX_CLIENTS];
};
static struct shared *shared;
static pid_t owner, children[MAX_CLIENTS];

static void reap(void) {
    if (getpid() != owner) return;
    for (int i = 0; i < MAX_CLIENTS; i++) {
        if (children[i] > 0) kill(children[i], SIGTERM);
    }
    for (int i = 0; i < MAX_CLIENTS; i++) {
        if (children[i] > 0) {
            while (waitpid(children[i], NULL, 0) < 0 && errno == EINTR) {}
        }
    }
}

static long now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000000000L + t.tv_nsec;
}

static void die(const char *what) {
    perror(what);
    exit(1);
}

// Waits until `fd` may be read, as `mode` waits; `q` is the client's kqueue or epoll.
static void await_readable(int fd, int q) {
    for (;;) {
        int r;
        switch (mode) {
        case SELECT_BLOCK:
        case SELECT: {
            fd_set set;
            FD_ZERO(&set);
            FD_SET(fd, &set);
            r = select(fd + 1, &set, NULL, NULL, NULL);
            break;
        }
        case POLL: {
            struct pollfd p = {fd, POLLIN, 0};
            r = poll(&p, 1, -1);
            break;
        }
#ifdef __APPLE__
        case KQUEUE: {
            struct kevent ev;
            r = kevent(q, NULL, 0, &ev, 1, NULL);
            break;
        }
#endif
#ifdef __linux__
        case EPOLL: {
            struct epoll_event ev;
            r = epoll_wait(q, &ev, 1, -1);
            break;
        }
#endif
        default:
            (void)q;
            return;
        }
        if (r > 0) return;
        if (r < 0 && errno != EINTR) die("waiting");
    }
}

static void *client(void *arg) {
    struct client *c = arg;
    char record[RECORD];
    int q = -1;
#ifdef __APPLE__
    if (mode == KQUEUE && (q = kqueue()) < 0) die("kqueue");
#endif
#ifdef __linux__
    if (mode == EPOLL && (q = epoll_create1(0)) < 0) die("epoll_create1");
#endif
    while (!atomic_load(&shared->stop)) {
        int fd = open(path, O_RDONLY | O_NONBLOCK);
        if (fd < 0) die("open");
        ssize_t n = read(fd, record, RECORD);
        if (n < 0 && errno == EAGAIN) {
            if (mode == SELECT_BLOCK || mode == READ) fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) & ~O_NONBLOCK);
#ifdef __APPLE__
            if (mode == KQUEUE) {
                struct kevent ev;
                EV_SET(&ev, fd, EVFILT_READ, EV_ADD | EV_CLEAR, 0, 0, NULL);
                if (kevent(q, &ev, 1, NULL, 0, NULL) < 0) die("kevent");
            }
#endif
#ifdef __linux__
            if (mode == EPOLL) {
                struct epoll_event ev = {.events = EPOLLIN | EPOLLET, .data.fd = fd};
                if (epoll_ctl(q, EPOLL_CTL_ADD, fd, &ev) < 0) die("epoll_ctl");
            }
#endif
            atomic_store(&c->since_ns, now_ns());
            atomic_store(&c->taken_then, atomic_load(&shared->taken));
            atomic_store(&c->woke, 0);
            for (;;) {
                atomic_store(&c->where, mode == READ ? 2 : 1);
                await_readable(fd, q);
                atomic_fetch_add(&c->woke, 1);
                atomic_store(&c->where, 2);
                n = read(fd, record, RECORD);
                if (n < 0 && (errno == EAGAIN || errno == EINTR)) continue;
                break;
            }
#ifdef __linux__
            if (mode == EPOLL) epoll_ctl(q, EPOLL_CTL_DEL, fd, NULL);
#endif
        }
        atomic_store(&c->where, 0);
        close(fd);  // closing it takes it out of the kqueue too
        if (n == RECORD) {
            atomic_fetch_add(&shared->taken, 1);
            char b = 1;
            if (write(told[1], &b, 1) != 1) die("told");
            if (pause_us > 0) usleep((useconds_t)pause_us);
        } else if (n > 0) {
            fprintf(stderr, "a short record: %zd\n", n);
            exit(1);
        } else if (n < 0) {
            die("read");
        }
    }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc < 5 || argc > 7) {
        fprintf(stderr, "usage: %s DIR CLIENTS SECONDS MODE [PAUSE_US] [threads|processes]\n", argv[0]);
        return 2;
    }
    int n = atoi(argv[2]);
    int seconds = atoi(argv[3]);
    pause_us = argc >= 6 ? atol(argv[5]) : 0;
    int processes = argc == 7 && strcmp(argv[6], "processes") == 0;
    if (argc == 7 && !processes && strcmp(argv[6], "threads")) return 2;
    int known = 0;
    for (int m = 0; m <= EPOLL; m++) {
        if (strcmp(argv[4], names[m]) == 0) {
            mode = (enum mode)m;
            known = 1;
        }
    }
#ifndef __APPLE__
    if (mode == KQUEUE) known = 0;
#endif
#ifndef __linux__
    if (mode == EPOLL) known = 0;
#endif
    if (n < 1 || n > MAX_CLIENTS || seconds < 1 || !known) {
        fprintf(stderr, "CLIENTS 1 to %d, SECONDS 1 or more, MODE one of this system's\n", MAX_CLIENTS);
        return 2;
    }
    snprintf(path, sizeof path, "%s/list", argv[1]);
    unlink(path);
    if (mkfifo(path, 0600) != 0 || pipe(told) != 0) die("mkfifo");
    // The maker's ends, as the daemon holds them: its own reader, then its writer.
    int back = open(path, O_RDONLY | O_NONBLOCK);
    int free_ = open(path, O_WRONLY);
    if (back < 0 || free_ < 0) die("open");
    shared = mmap(NULL, sizeof *shared, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANON, -1, 0);
    if (shared == MAP_FAILED) die("shared state");
    owner = getpid();
    if (atexit(reap) != 0) die("atexit");
    pthread_t threads[MAX_CLIENTS];
    for (int i = 0; i < n; i++) {
        if (processes) {
            pid_t pid = fork();
            if (pid < 0) die("fork");
            if (pid == 0) {
                close(back);
                close(free_);
                close(told[0]);
                client(&shared->clients[i]);
                _exit(0);
            }
            children[i] = pid;
        } else if (pthread_create(&threads[i], NULL, client, &shared->clients[i]) != 0) {
            die("pthread_create");
        }
    }
    long end = now_ns() + (long)seconds * 1000000000L;
    long stuck = 0;
    long max_wait = 0;
    char record[RECORD];
    memset(record, '0', RECORD - 1);
    record[RECORD - 1] = '\n';
    while (now_ns() < end) {
        int waiting = 0;
        ioctl(back, FIONREAD, &waiting);
        while (waiting / RECORD < 2) {
            if (write(free_, record, RECORD) != RECORD) die("offer");
            waiting += RECORD;
        }
        // Told of a take, or a 100 ms look at the clients.
        fd_set set;
        FD_ZERO(&set);
        FD_SET(told[0], &set);
        struct timeval tv = {0, 100000};
        if (select(told[0] + 1, &set, NULL, NULL, &tv) > 0) {
            char b[64];
            if (read(told[0], b, sizeof b) < 0) die("told");
        }
        long t = now_ns();
        for (int i = 0; i < n; i++) {
            struct client *c = &shared->clients[i];
            int where = atomic_load(&c->where);
            if (where == 0) continue;
            long waited = t - atomic_load(&c->since_ns);
            if (waited > max_wait) max_wait = waited;
            long others = atomic_load(&shared->taken) - atomic_load(&c->taken_then);
            if (waited > 5000000000L && others >= 100 && c->stuck_reported != atomic_load(&c->since_ns)) {
                c->stuck_reported = atomic_load(&c->since_ns);
                stuck++;
                printf("client %d waited %.1f s %s while the others took %ld, its wait returning %ld times\n",
                       i, waited / 1e9, where == 1 ? "waiting" : "in its read", others,
                       atomic_load(&c->woke));
                fflush(stdout);
            }
        }
    }
    atomic_store(&shared->stop, 1);
    printf("%s, %s, %d clients, %ld us apart, %d s: %ld records taken, %ld clients stuck, longest wait %.3f s\n",
           names[mode], processes ? "processes" : "threads", n, pause_us, seconds, atomic_load(&shared->taken), stuck, max_wait / 1e9);
    fflush(stdout);
    // The clients still waiting are left to the process's exit.
    unlink(path);
    reap();
    _exit(0);
}
