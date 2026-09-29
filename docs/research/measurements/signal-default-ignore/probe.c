/* Does a blocked signal whose default action is to ignore it stay pending, for sigwait(3),
 * under SIG_DFL and under a handler? One process per case: SIGWINCH, SIGIO, SIGCONT, and
 * SIGINT for contrast (platform-measurements.md M31).
 *
 *     cc -O2 -o probe probe.c && ./probe
 *     zig cc -target aarch64-linux-musl -static -O2 -o probe-linux probe.c   (for Linux)
 */
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void nothing(int s) { (void)s; }

static int probe(int sig, int handler) {
    pid_t pid = fork();
    if (pid == 0) {
        sigset_t set; sigemptyset(&set); sigaddset(&set, sig);
        pthread_sigmask(SIG_BLOCK, &set, NULL);
        struct sigaction sa; memset(&sa, 0, sizeof sa);
        sa.sa_handler = handler ? nothing : SIG_DFL;
        sigaction(sig, &sa, NULL);
        kill(getpid(), sig);
        sigset_t pending; sigpending(&pending);
        _exit(sigismember(&pending, sig) ? 0 : 1);
    }
    int st; waitpid(pid, &st, 0);
    return WIFEXITED(st) && WEXITSTATUS(st) == 0;
}

int main(void) {
    int sigs[] = {SIGWINCH, SIGIO, SIGCONT, SIGINT};
    const char *names[] = {"SIGWINCH", "SIGIO", "SIGCONT", "SIGINT"};
    for (int i = 0; i < 4; i++)
        printf("%-8s blocked, SIG_DFL: %s; with a handler: %s\n", names[i],
               probe(sigs[i], 0) ? "pending" : "discarded",
               probe(sigs[i], 1) ? "pending" : "discarded");
    return 0;
}
