/* The cost of looking for a free descriptor as the daemon does before an accept on macOS
 * (daemon.rs, descriptor_free): fcntl(F_DUPFD_CLOEXEC) of a socket, then close of the copy,
 * 100,000 times, each timed on the raw uptime clock (PM M99).
 *
 *     cc -O2 -o dupcost dupcost.c && ./dupcost
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>
static int cmp(const void *a, const void *b) { long x = *(const long *)a, y = *(const long *)b; return (x > y) - (x < y); }
int main(void) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    enum { N = 100000 };
    static long ns[N];
    for (int i = 0; i < N; i++) {
        unsigned long long a, b;
        a = clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
        int c = fcntl(fd, F_DUPFD_CLOEXEC, 0);
        close(c);
        b = clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
        ns[i] = (long)(b - a);
    }
    qsort(ns, N, sizeof ns[0], cmp);
    printf("n %d p50 %ld p90 %ld p99 %ld max %ld ns\n", N, ns[N / 2], ns[N * 9 / 10], ns[N * 99 / 100], ns[N - 1]);
    return 0;
}
