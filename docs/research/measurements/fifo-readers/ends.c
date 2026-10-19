// How clients waiting on an empty FIFO in kqueue(2) learn its writer has gone (macOS, PM
// M262), as a home's clients wait on its list of request slots: neither poll(2) nor
// kqueue(2) says a FIFO's writer has ended (M253), so a client also watches the FIFO's
// name (EVFILT_VNODE: NOTE_DELETE, NOTE_RENAME, NOTE_REVOKE) and the writer's process
// (EVFILT_PROC: NOTE_EXIT).
//
//   cc -O2 -o ends ends.c && ./ends DIR CLIENTS ROUNDS HOW
//
// Each round, a writer process makes the FIFO DIR/list and holds both its ends, as a daemon
// holds its list; CLIENTS processes open it, find nothing, and wait in kevent(2) for any of
// the three. Then the writer ends as HOW says: `unlink` removes the name and goes on
// holding its ends, as a closing daemon does (`slots::unoffer`); `exit` ends its process,
// the name left, as a daemon killed does; `both` removes the name, then exits. Says how many
// clients woke within 2 s, and of what, over all rounds.
#ifdef __APPLE__
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void die(const char *what) {
    perror(what);
    exit(1);
}

int main(int argc, char **argv) {
    if (argc != 5) {
        fprintf(stderr, "usage: %s DIR CLIENTS ROUNDS unlink|exit|both\n", argv[0]);
        return 2;
    }
    int clients = atoi(argv[2]);
    int rounds = atoi(argv[3]);
    const char *how = argv[4];
    if (clients < 1 || clients > 256 || rounds < 1 ||
        (strcmp(how, "unlink") && strcmp(how, "exit") && strcmp(how, "both"))) {
        fprintf(stderr, "CLIENTS 1 to 256, ROUNDS 1 or more, HOW unlink, exit or both\n");
        return 2;
    }
    char path[1024];
    snprintf(path, sizeof path, "%s/list", argv[1]);
    long woke = 0, by_read = 0, by_vnode = 0, by_proc = 0, late = 0;
    for (int round = 0; round < rounds; round++) {
        unlink(path);
        if (mkfifo(path, 0600) != 0) die("mkfifo");
        int go[2], ready[2], report[2];
        if (pipe(go) || pipe(ready) || pipe(report)) die("pipe");
        pid_t writer = fork();
        if (writer < 0) die("fork");
        if (writer == 0) {
            int back = open(path, O_RDONLY | O_NONBLOCK);
            int w = open(path, O_WRONLY);
            if (back < 0 || w < 0) die("writer's open");
            char b;
            if (write(ready[1], "w", 1) != 1 || read(go[0], &b, 1) != 1) die("writer's pipes");
            if (strcmp(how, "exit") != 0) unlink(path);
            if (strcmp(how, "unlink") == 0) {
                // Holds its ends until told to go, as a closing daemon serves on.
                if (read(go[0], &b, 1) < 0) die("writer's wait");
            }
            _exit(0);
        }
        char b;
        if (read(ready[0], &b, 1) != 1) die("ready");
        for (int i = 0; i < clients; i++) {
            pid_t c = fork();
            if (c < 0) die("fork");
            if (c == 0) {
                int fd = open(path, O_RDONLY | O_NONBLOCK);
                if (fd < 0) die("client's open");
                char rec[16];
                if (read(fd, rec, sizeof rec) >= 0 || errno != EAGAIN) die("not empty");
                int q = kqueue();
                struct kevent ev[3];
                EV_SET(&ev[0], fd, EVFILT_READ, EV_ADD | EV_CLEAR, 0, 0, NULL);
                EV_SET(&ev[1], fd, EVFILT_VNODE, EV_ADD | EV_CLEAR, NOTE_DELETE | NOTE_RENAME | NOTE_REVOKE, 0, NULL);
                EV_SET(&ev[2], writer, EVFILT_PROC, EV_ADD | EV_CLEAR, NOTE_EXIT, 0, NULL);
                if (q < 0 || kevent(q, ev, 3, NULL, 0, NULL) < 0) die("kevent");
                if (write(ready[1], "c", 1) != 1) die("client's ready");
                struct kevent got;
                struct timespec ts = {4, 0};
                int n = kevent(q, NULL, 0, &got, 1, &ts);
                char what = n <= 0 ? '-' : got.filter == EVFILT_READ ? 'r' : got.filter == EVFILT_VNODE ? 'v' : 'p';
                if (write(report[1], &what, 1) != 1) die("client's report");
                _exit(0);
            }
        }
        for (int i = 0; i < clients; i++) {
            if (read(ready[0], &b, 1) != 1) die("clients' ready");
        }
        struct timespec t0, t1;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        if (write(go[1], "g", 1) != 1) die("go");
        for (int i = 0; i < clients; i++) {
            char what;
            if (read(report[0], &what, 1) != 1) die("report");
            clock_gettime(CLOCK_MONOTONIC, &t1);
            double s = (t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) / 1e9;
            if (what == '-' || s > 2.0) {
                late++;
            } else {
                woke++;
                by_read += what == 'r';
                by_vnode += what == 'v';
                by_proc += what == 'p';
            }
        }
        if (strcmp(how, "unlink") == 0 && write(go[1], "g", 1) != 1) die("go again");
        while (wait(NULL) > 0) {
        }
        close(go[0]), close(go[1]), close(ready[0]), close(ready[1]), close(report[0]), close(report[1]);
    }
    unlink(path);
    printf("%s, %d clients, %d rounds: %ld woke within 2 s (%ld by EVFILT_READ, %ld by EVFILT_VNODE, %ld by "
           "EVFILT_PROC), %ld did not\n",
           how, clients, rounds, woke, by_read, by_vnode, by_proc, late);
    return late != 0;
}
#else
#include <stdio.h>
int main(void) {
    fprintf(stderr, "macOS only: elsewhere a FIFO's reader learns of its writer's end (M253)\n");
    return 2;
}
#endif
