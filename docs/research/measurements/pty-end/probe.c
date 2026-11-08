// What a pty's controller reads of what a command wrote to its replica before it ended,
// when the controller's reader is late (PM M246): a reader held back, as a client's
// command's output is while the client reads slowly, or a step's while its stream is full.
//
//   probe WRITER STRATEGY DELAY_MS BYTES
//
// WRITER `plain` writes and closes the replica, as a build does, whose terminal is not its
// controlling one; `leader` is a child process leading a session whose controlling
// terminal the replica is, as a client's command does, which writes and exits.
// STRATEGY `last`: the writer's close of the replica is its last, as a daemon's terminal
// for a client's command had it. `kept`: a writer of the replica's own is kept, and
// closed once the command has ended and TIOCOUTQ says nothing is unread, by the waiter
// where nothing is, else by the reader once it has read it (daemon/remote.rs, Kept).
// `gone`, `gone-flushed`: kept, but no reader comes, as for a client gone; at DELAY_MS the
// kept writer is let go, closed, or its output queue flushed (TCOFLUSH) and then closed.
//
// Prints the bytes written and read, TIOCOUTQ once written, and how long the writer took
// to close the replica or, a leader, to end.
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>
#if defined(__APPLE__)
#include <util.h>
#else
#include <pty.h>
#endif

static int controller, delay_ms, kept, gone, flushed;
static long got;
static atomic_int keeper = -1;
static atomic_bool ended;

static double now(void) {
    struct timeval t;
    gettimeofday(&t, 0);
    return t.tv_sec + t.tv_usec / 1e6;
}

static int unread(int fd) {
    int n = 0;
    return ioctl(fd, TIOCOUTQ, &n) == 0 ? n : 0;
}

// Closes the kept writer, once, where nothing it holds is unread.
static void close_if_read(void) {
    int k = atomic_load(&keeper);
    if (k >= 0 && unread(k) == 0 && atomic_compare_exchange_strong(&keeper, &k, -1)) {
        close(k);
    }
}

static void *reader(void *arg) {
    (void)arg;
    usleep(delay_ms * 1000);
    char buf[4096];
    for (;;) {
        ssize_t n = read(controller, buf, sizeof buf);
        if (n > 0) {
            got += n;
            if (kept && atomic_load(&ended)) close_if_read();
            continue;
        }
        if (n < 0 && errno == EINTR) continue;
        break;  // 0, or EIO: Linux's end (drivers/tty/n_tty.c, n_tty_read)
    }
    return 0;
}

// The client gone: at DELAY_MS the kept writer is let go.
static void *abandoner(void *arg) {
    (void)arg;
    usleep(delay_ms * 1000);
    int k = atomic_exchange(&keeper, -1);
    if (k >= 0) {
        if (flushed) tcflush(k, TCOFLUSH);
        close(k);
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 5) {
        fprintf(stderr, "usage: probe plain|leader last|kept|gone|gone-flushed DELAY_MS BYTES\n");
        return 2;
    }
    int leader = strcmp(argv[1], "leader") == 0;
    gone = strncmp(argv[2], "gone", 4) == 0;
    flushed = strcmp(argv[2], "gone-flushed") == 0;
    kept = gone || strcmp(argv[2], "kept") == 0;
    delay_ms = atoi(argv[3]);
    int bytes = atoi(argv[4]);
    int replica;
    if (openpty(&controller, &replica, 0, 0, 0) < 0) {
        perror("openpty");
        return 1;
    }
    struct termios t;
    tcgetattr(replica, &t);
    t.c_oflag &= ~OPOST;
    tcsetattr(replica, TCSANOW, &t);
    if (kept) atomic_store(&keeper, dup(replica));
    pthread_t r;
    pthread_create(&r, 0, gone ? abandoner : reader, 0);
    char *data = malloc(bytes);
    memset(data, 'x', bytes);
    ssize_t wrote = bytes;
    int queued = -1;
    double t0 = now(), took;
    if (leader) {
        pid_t pid = fork();
        if (pid == 0) {
            close(controller);
            int k = atomic_load(&keeper);
            if (k >= 0) close(k);
            setsid();
            ioctl(replica, TIOCSCTTY, 0);
            _exit(write(replica, data, bytes) == bytes ? 0 : 1);
        }
        close(replica);
        int status;
        waitpid(pid, &status, 0);
        took = now() - t0;
        wrote = WIFEXITED(status) && WEXITSTATUS(status) == 0 ? bytes : -1;
    } else {
        wrote = write(replica, data, bytes);
        queued = unread(replica);
        t0 = now();
        close(replica);
        took = now() - t0;
    }
    if (kept && !gone) {
        atomic_store(&ended, 1);
        close_if_read();
    }
    pthread_join(r, 0);
    printf("%s %s delay_ms=%d wrote=%zd queued=%d end_s=%.3f read=%ld\n", argv[1], argv[2], delay_ms, wrote,
           queued, took, got);
    return 0;
}
