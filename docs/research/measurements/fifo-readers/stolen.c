// One client waiting on a FIFO, whose record another reader takes as it wakes (PM M262):
// does it still wake for the next? The case readers.c's stuck select(2) clients suggest,
// with no other client in the way.
//
//   cc -O2 -Wall -Wextra -Werror -pthread -o stolen stolen.c && ./stolen DIR MODE ROUNDS SETTLE_US
//
// Each round a new client opens the FIFO DIR/list not waiting, finds nothing, and waits as
// MODE says (select, poll or kqueue), then reads, not waiting. The maker writes a record and
// takes it back at once through its own reader, as another client takes one first; waits
// SETTLE_US; writes a second record, which stays; and says whether the client took it
// within 1 s. A round whose client took the first record itself is counted apart.
#ifdef __APPLE__
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/select.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define RECORD 16

static char path[1024];
static const char *mode;
// 0 starting, 1 waiting, 2 took a record, 3 failed.
static atomic_int state;

static void *client(void *arg) {
    (void)arg;
    char record[RECORD];
    int fd = open(path, O_RDONLY | O_NONBLOCK);
    if (fd < 0 || read(fd, record, RECORD) >= 0 || errno != EAGAIN) {
        atomic_store(&state, 3);
        if (fd >= 0) close(fd);
        return NULL;
    }
    int kq = -1;
    if (!strcmp(mode, "kqueue")) {
        struct kevent ev;
        EV_SET(&ev, fd, EVFILT_READ, EV_ADD, 0, 0, NULL);
        if ((kq = kqueue()) < 0 || kevent(kq, &ev, 1, NULL, 0, NULL) < 0) {
            atomic_store(&state, 3);
            close(fd);
            return NULL;
        }
    }
    atomic_store(&state, 1);
    for (;;) {
        int r;
        if (!strcmp(mode, "select")) {
            fd_set set;
            FD_ZERO(&set);
            FD_SET(fd, &set);
            r = select(fd + 1, &set, NULL, NULL, NULL);
        } else if (!strcmp(mode, "poll")) {
            struct pollfd p = {fd, POLLIN, 0};
            r = poll(&p, 1, -1);
        } else {
            struct kevent ev;
            r = kevent(kq, NULL, 0, &ev, 1, NULL);
        }
        if (r < 0 && errno != EINTR) {
            atomic_store(&state, 3);
            break;
        }
        ssize_t n = read(fd, record, RECORD);
        if (n == RECORD) {
            atomic_store(&state, 2);
            break;
        }
        if (n < 0 && (errno == EAGAIN || errno == EINTR)) continue;
        atomic_store(&state, 3);
        break;
    }
    close(fd);
    if (kq >= 0) close(kq);
    return NULL;
}

static void nap(long us) {
    struct timespec t = {us / 1000000, (us % 1000000) * 1000};
    nanosleep(&t, NULL);
}

static void die(const char *what) {
    perror(what);
    exit(1);
}

int main(int argc, char **argv) {
    if (argc != 5) {
        fprintf(stderr, "usage: %s DIR select|poll|kqueue ROUNDS SETTLE_US\n", argv[0]);
        return 2;
    }
    snprintf(path, sizeof path, "%s/list", argv[1]);
    mode = argv[2];
    int rounds = atoi(argv[3]);
    long settle = atol(argv[4]);
    if ((strcmp(mode, "select") && strcmp(mode, "poll") && strcmp(mode, "kqueue")) || rounds < 1 || settle < 0) {
        fprintf(stderr, "MODE select, poll or kqueue; ROUNDS 1 or more; SETTLE_US 0 or more\n");
        return 2;
    }
    unlink(path);
    if (mkfifo(path, 0600) != 0) die("mkfifo");
    // The maker's ends, as the daemon holds them: its own reader, then its writer.
    int back = open(path, O_RDONLY | O_NONBLOCK);
    int w = open(path, O_WRONLY);
    if (back < 0 || w < 0) die("open");
    char record[RECORD], taken[RECORD];
    memset(record, '0', RECORD - 1);
    record[RECORD - 1] = '\n';
    int woke = 0, slept = 0, raced = 0, failed = 0;
    for (int i = 0; i < rounds; i++) {
        atomic_store(&state, 0);
        pthread_t t;
        if (pthread_create(&t, NULL, client, NULL) != 0) die("pthread_create");
        while (atomic_load(&state) == 0) nap(100);
        nap(settle);
        if (write(w, record, RECORD) != RECORD) die("write");
        ssize_t back_took = read(back, taken, RECORD);
        nap(settle);
        if (atomic_load(&state) == 3) {
            failed++;
            pthread_join(t, NULL);
            continue;
        }
        if (back_took != RECORD || atomic_load(&state) == 2) {
            // The client took the first itself: not the case asked about.
            raced++;
            if (atomic_load(&state) != 2 && write(w, record, RECORD) != RECORD) die("write");
            pthread_join(t, NULL);
            while (read(back, taken, RECORD) == RECORD) {}
            continue;
        }
        if (write(w, record, RECORD) != RECORD) die("write");
        for (int ms = 0; atomic_load(&state) == 1 && ms < 1000; ms++) nap(1000);
        if (atomic_load(&state) == 2) {
            woke++;
            pthread_join(t, NULL);
        } else {
            // Left waiting to the process's exit; its record taken back for the next round.
            slept++;
            pthread_detach(t);
            if (read(back, taken, RECORD) != RECORD) die("taking back");
        }
    }
    printf("%s, %ld us to settle: %d rounds, %d woke for the second record, %d slept through it 1 s, "
           "%d took the first themselves, %d failed\n",
           mode, settle, rounds, woke, slept, raced, failed);
    unlink(path);
    return 0;
}
#else
#include <stdio.h>
int main(void) {
    fprintf(stderr, "macOS only: on Linux every reader of a FIFO sees each record (M262)\n");
    return 2;
}
#endif
