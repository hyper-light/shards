// M254 (docs/research/platform-measurements.md): what a client's request to its daemon costs
// through files in the home (D185), against a Unix socket's, as the client sees it: from
// its first call to its reading the daemon's first answer. A daemon process answers each
// request with a byte. The four ways are interleaved, a round of each in turn, its order
// rotating, so that each meets the same host.
//
// socket: connect(2) to the daemon's socket, a 64-byte request, the answer.
// dir:    the client makes its request's directory: mkdir requests/ID, mkfifo answer and
//         say, its ends of each opened, the request written to request.new and renamed
//         request, ID written to the daemon's door FIFO; the daemon reads the ID and the
//         request, opens its ends and answers. Then the client removes its directory (its
//         end, measured apart: a client does it as it ends).
// slot:   the daemon made the request's directory ahead, its FIFOs in it, holding its own
//         ends: say's reader and a writer of its own, answer's writer. The client takes
//         its lock (flock(2)), opens say to write and answer to read, writes the request
//         and reads the answer; then closes its ends.
// pair:   one FIFO pair, opened ahead by both: the floor a FIFO's round trip sets.
//
//   requests DIR N
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *dir;
static int n;

static double now_us(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
}

static int cmp(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

static void said(const char *name, double *v) {
    qsort(v, n, sizeof *v, cmp);
    printf("%-14s n=%d p50=%.1fus p90=%.1fus p99=%.1fus max=%.1fus\n", name, n, v[n / 2],
           v[n * 9 / 10], v[n * 99 / 100], v[n - 1]);
}

static int must(int r, const char *what) {
    if (r < 0) {
        perror(what);
        exit(1);
    }
    return r;
}

static void path(char *out, size_t len, const char *a, int i, const char *b) {
    if (i < 0) snprintf(out, len, "%s/%s", dir, a);
    else snprintf(out, len, "%s/%s/%d%s", dir, a, i, b);
}

// A door: a FIFO its daemon reads, holding a writer of its own so that it never ends.
static int door(const char *at) {
    unlink(at);
    must(mkfifo(at, 0600), "mkfifo");
    int r = must(open(at, O_RDONLY | O_NONBLOCK), "door");
    must(open(at, O_WRONLY), "door's own writer");
    must(fcntl(r, F_SETFL, 0), "fcntl");
    return r;
}

static void socket_daemon(int rounds) {
    char p[600];
    path(p, sizeof p, "d.sock", -1, "");
    int l = must(socket(AF_UNIX, SOCK_STREAM, 0), "socket");
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    snprintf(a.sun_path, sizeof a.sun_path, "%s", p);
    unlink(p);
    must(bind(l, (struct sockaddr *)&a, sizeof a), "bind");
    must(listen(l, 64), "listen");
    for (int i = 0; i < rounds; i++) {
        int c = must(accept(l, NULL, NULL), "accept");
        char req[64];
        if (read(c, req, sizeof req) != sizeof req) _exit(2);
        if (write(c, "k", 1) != 1) _exit(3);
        close(c);
    }
    _exit(0);
}

static double socket_client(void) {
    char p[600];
    path(p, sizeof p, "d.sock", -1, "");
    double t = now_us();
    int c = must(socket(AF_UNIX, SOCK_STREAM, 0), "socket");
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    snprintf(a.sun_path, sizeof a.sun_path, "%s", p);
    must(connect(c, (struct sockaddr *)&a, sizeof a), "connect");
    char req[64] = {0}, k;
    if (write(c, req, sizeof req) != sizeof req) exit(4);
    if (read(c, &k, 1) != 1) exit(5);
    double took = now_us() - t;
    close(c);
    return took;
}

static void dir_daemon(int rounds) {
    char p[600];
    path(p, sizeof p, "dir.door", -1, "");
    FILE *ids = fdopen(door(p), "r");
    char line[128];
    for (int i = 0; i < rounds; i++) {
        if (!fgets(line, sizeof line, ids)) _exit(6);
        int id = atoi(line);
        path(p, sizeof p, "requests", id, "/request");
        int q = must(open(p, O_RDONLY), "request");
        char req[64];
        if (read(q, req, sizeof req) != sizeof req) _exit(7);
        close(q);
        path(p, sizeof p, "requests", id, "/answer");
        int a = must(open(p, O_WRONLY | O_NONBLOCK), "answer");
        path(p, sizeof p, "requests", id, "/say");
        int s = must(open(p, O_RDONLY | O_NONBLOCK), "say");
        if (write(a, "k", 1) != 1) _exit(8);
        close(a);
        close(s);
    }
    _exit(0);
}

static double dir_client(int id, double *end) {
    char d[600], p[700], q[700];
    double t = now_us();
    path(d, sizeof d, "requests", id, "");
    must(mkdir(d, 0700), "mkdir");
    snprintf(p, sizeof p, "%s/answer", d);
    must(mkfifo(p, 0600), "mkfifo answer");
    int ar = must(open(p, O_RDONLY | O_NONBLOCK), "answer");
    int aw = must(open(p, O_WRONLY), "answer's own writer");
    must(fcntl(ar, F_SETFL, 0), "fcntl");
    snprintf(p, sizeof p, "%s/say", d);
    must(mkfifo(p, 0600), "mkfifo say");
    int sr = must(open(p, O_RDONLY | O_NONBLOCK), "say's own reader");
    int sw = must(open(p, O_WRONLY), "say");
    snprintf(p, sizeof p, "%s/request.new", d);
    snprintf(q, sizeof q, "%s/request", d);
    int f = must(open(p, O_WRONLY | O_CREAT | O_EXCL, 0600), "request.new");
    char req[64] = {0};
    if (write(f, req, sizeof req) != sizeof req) exit(9);
    close(f);
    must(rename(p, q), "rename");
    path(p, sizeof p, "dir.door", -1, "");
    int dw = must(open(p, O_WRONLY | O_NONBLOCK), "door");
    char line[32];
    int len = snprintf(line, sizeof line, "%d\n", id);
    if (write(dw, line, len) != len) exit(10);
    close(dw);
    char k;
    if (read(ar, &k, 1) != 1) exit(11);
    double took = now_us() - t;
    t = now_us();
    close(ar);
    close(aw);
    close(sr);
    close(sw);
    snprintf(p, sizeof p, "%s/answer", d);
    unlink(p);
    snprintf(p, sizeof p, "%s/say", d);
    unlink(p);
    unlink(q);
    rmdir(d);
    *end = now_us() - t;
    return took;
}

// The slots, made ahead: each a directory with its lock, say and answer; the daemon holds
// say's reader and a writer of its own, and answer's writer.
static void slot_daemon(int rounds, int ready) {
    char p[700];
    int *say = calloc(rounds, sizeof *say), *answer = calloc(rounds, sizeof *answer);
    for (int i = 0; i < rounds; i++) {
        path(p, sizeof p, "slots", i, "");
        must(mkdir(p, 0700), "mkdir slot");
        path(p, sizeof p, "slots", i, "/lock");
        close(must(open(p, O_RDONLY | O_CREAT, 0600), "lock"));
        path(p, sizeof p, "slots", i, "/say");
        must(mkfifo(p, 0600), "mkfifo say");
        say[i] = must(open(p, O_RDONLY | O_NONBLOCK), "say");
        must(open(p, O_WRONLY), "say's own writer");
        must(fcntl(say[i], F_SETFL, 0), "fcntl");
        path(p, sizeof p, "slots", i, "/answer");
        must(mkfifo(p, 0600), "mkfifo answer");
        int r = must(open(p, O_RDONLY | O_NONBLOCK), "answer's own reader");
        answer[i] = must(open(p, O_WRONLY), "answer");
        (void)r;
    }
    if (write(ready, "r", 1) != 1) _exit(14);
    for (int i = 0; i < rounds; i++) {
        char req[64];
        if (read(say[i], req, sizeof req) != sizeof req) _exit(15);
        if (write(answer[i], "k", 1) != 1) _exit(16);
    }
    _exit(0);
}

static double slot_client(int i) {
    char p[700];
    double t = now_us();
    path(p, sizeof p, "slots", i, "/lock");
    int l = must(open(p, O_RDONLY), "lock");
    must(flock(l, LOCK_EX | LOCK_NB), "flock");
    path(p, sizeof p, "slots", i, "/answer");
    int a = must(open(p, O_RDONLY), "answer");
    path(p, sizeof p, "slots", i, "/say");
    int s = must(open(p, O_WRONLY), "say");
    char req[64] = {0}, k;
    if (write(s, req, sizeof req) != sizeof req) exit(17);
    if (read(a, &k, 1) != 1) exit(18);
    double took = now_us() - t;
    close(s);
    close(a);
    close(l);
    return took;
}

static void pair_daemon(int rounds) {
    char p[600], q[600];
    path(p, sizeof p, "pair.ask", -1, "");
    path(q, sizeof q, "pair.answer", -1, "");
    int r = door(p);
    int w = must(open(q, O_WRONLY), "pair.answer");
    for (int i = 0; i < rounds; i++) {
        char req[64];
        if (read(r, req, sizeof req) != sizeof req) _exit(19);
        if (write(w, "k", 1) != 1) _exit(20);
    }
    _exit(0);
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: requests DIR N\n");
        return 2;
    }
    dir = argv[1];
    n = atoi(argv[2]);
    int warm = 100, rounds = n + warm;
    char p[600];
    path(p, sizeof p, "requests", -1, "");
    must(mkdir(p, 0700), "requests");
    path(p, sizeof p, "slots", -1, "");
    must(mkdir(p, 0700), "slots");
    path(p, sizeof p, "pair.answer", -1, "");
    must(mkfifo(p, 0600), "pair.answer");
    int ready[2];
    must(pipe(ready), "pipe");
    fflush(stdout);
    pid_t d[4];
    if ((d[0] = fork()) == 0) socket_daemon(rounds);
    if ((d[1] = fork()) == 0) dir_daemon(rounds);
    if ((d[2] = fork()) == 0) slot_daemon(rounds, ready[1]);
    if ((d[3] = fork()) == 0) pair_daemon(rounds);
    char c;
    if (read(ready[0], &c, 1) != 1) return 1;
    // The daemons' doors and socket are there once their files are.
    struct stat st;
    path(p, sizeof p, "d.sock", -1, "");
    while (stat(p, &st)) usleep(1000);
    path(p, sizeof p, "dir.door", -1, "");
    while (stat(p, &st)) usleep(1000);
    path(p, sizeof p, "pair.ask", -1, "");
    while (stat(p, &st)) usleep(1000);
    usleep(100000);
    path(p, sizeof p, "pair.answer", -1, "");
    int pr = must(open(p, O_RDONLY), "pair.answer");
    path(p, sizeof p, "pair.ask", -1, "");
    int pw = must(open(p, O_WRONLY), "pair.ask");
    double *v[5];
    for (int k = 0; k < 5; k++) v[k] = calloc(rounds, sizeof(double));
    for (int i = 0; i < rounds; i++) {
        for (int j = 0; j < 4; j++) {
            int way = (i + j) % 4;
            double t, end = 0;
            if (way == 0) t = socket_client();
            else if (way == 1) t = dir_client(i, &end);
            else if (way == 2) t = slot_client(i);
            else {
                t = now_us();
                char req[64] = {0}, k;
                if (write(pw, req, sizeof req) != sizeof req) return 21;
                if (read(pr, &k, 1) != 1) return 22;
                t = now_us() - t;
            }
            if (i >= warm) {
                v[way][i - warm] = t;
                if (way == 1) v[4][i - warm] = end;
            }
        }
    }
    for (int k = 0; k < 4; k++) waitpid(d[k], NULL, 0);
    said("socket", v[0]);
    said("dir", v[1]);
    said("dir, its end", v[4]);
    said("slot", v[2]);
    said("pair", v[3]);
    return 0;
}
