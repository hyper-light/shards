// M253 (docs/research/platform-measurements.md), part 3: does a FIFO's reader, having just
// read its writer's data, ever sleep through the writer's close (as M236's reader of a Unix
// stream socket can)? The writer writes a byte and closes at once, or after a random delay
// of 0 to 50 us; the reader reads the byte, then waits for the end: in a blocking read(2),
// or in select(2) then a read. A wait not over 1.1 s later is stuck. Run 8 at once: the
// race wants a loaded host.
//
//   race PATH read|select N
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>
static void ring(int s) { (void)s; }
int main(int argc, char **argv) {
    const char *path = argv[1], *how = argv[2];
    int n = atoi(argv[3]);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = ring;
    sigaction(SIGALRM, &sa, NULL);
    int stuck = 0, ended = 0, other = 0;
    srand(11);
    for (int i = 0; i < n; i++) {
        unlink(path);
        mkfifo(path, 0600);
        int go[2];
        pipe(go);
        int delay = rand() % 51;
        pid_t pid = fork();
        if (pid == 0) {
            int w = open(path, O_WRONLY);
            char c;
            if (read(go[0], &c, 1) != 1) _exit(3);
            if (write(w, "x", 1) != 1) _exit(4);
            if (delay) usleep(delay);
            close(w);
            _exit(0);
        }
        int r = open(path, O_RDONLY);
        if (write(go[1], "g", 1) != 1) return 1;
        char c;
        if (read(r, &c, 1) != 1) { other++; goto next; }
        struct itimerval t = {{0, 0}, {1, 100000}};
        setitimer(ITIMER_REAL, &t, NULL);
        ssize_t got = -1;
        if (!strcmp(how, "read")) {
            got = read(r, &c, 1);
        } else {
            fd_set s;
            FD_ZERO(&s);
            FD_SET(r, &s);
            if (select(r + 1, &s, NULL, NULL, NULL) > 0) got = read(r, &c, 1);
        }
        int e = errno;
        struct itimerval off = {{0, 0}, {0, 0}};
        setitimer(ITIMER_REAL, &off, NULL);
        if (got == 0) ended++;
        else if (got < 0 && e == EINTR) stuck++;
        else other++;
    next:
        waitpid(pid, NULL, 0);
        close(r);
        close(go[0]);
        close(go[1]);
    }
    unlink(path);
    printf("%-6s n=%d: the end seen %d, stuck past 1.1 s %d, other %d\n", how, n, ended, stuck, other);
    return 0;
}
