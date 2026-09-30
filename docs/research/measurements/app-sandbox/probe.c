// What a command-line tool signed into App Sandbox may do (PM M67): each argument pair is
// one trial, and each prints "trial: ok" or "trial: refused (errno)". The harness
// (run.sh) signs it with entitlements.plist and runs it with descriptors and bookmarks
// from an unsandboxed parent.
#include <CoreFoundation/CoreFoundation.h>
#include <Hypervisor/Hypervisor.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <netinet/in.h>
#include <time.h>
#include <unistd.h>

static void said(const char *what, int ok) {
    if (ok) printf("%s: ok\n", what);
    else printf("%s: refused (%s)\n", what, strerror(errno));
}

static int unix_socket(const char *path, int do_bind) {
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s < 0) return 0;
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    strncpy(a.sun_path, path, sizeof a.sun_path - 1);
    int r = do_bind ? bind(s, (struct sockaddr *)&a, sizeof a) : connect(s, (struct sockaddr *)&a, sizeof a);
    close(s);
    return r == 0;
}

// A bookmark, hex-encoded, resolved and its sandbox extension started; then `path` opened.
static int bookmark(const char *hex, const char *path, int flags) {
    size_t n = strlen(hex) / 2;
    UInt8 *b = malloc(n);
    for (size_t i = 0; i < n; i++) sscanf(hex + 2 * i, "%2hhx", &b[i]);
    CFDataRef d = CFDataCreate(NULL, b, (CFIndex)n);
    Boolean stale = false;
    CFErrorRef err = NULL;
    CFURLRef u = CFURLCreateByResolvingBookmarkData(NULL, d, 0, NULL, NULL, &stale, &err);
    if (!u) { errno = EPERM; return 0; }
    Boolean started = CFURLStartAccessingSecurityScopedResource(u);
    int fd = open(path, flags, 0600);
    int ok = fd >= 0;
    if (fd >= 0) close(fd);
    printf("  (resolved, scope started %d)\n", started);
    return ok;
}

int main(int argc, char **argv) {
    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    printf("HOME=%s\n", getenv("HOME") ? getenv("HOME") : "");
    for (int i = 1; i + 1 < argc; i += 2) {
        const char *op = argv[i], *arg = argv[i + 1];
        char what[1200];
        snprintf(what, sizeof what, "%s %s", op, arg);
        if (!strcmp(op, "read")) { int fd = open(arg, O_RDONLY); said(what, fd >= 0); if (fd >= 0) close(fd); }
        else if (!strcmp(op, "write")) { int fd = open(arg, O_WRONLY | O_CREAT, 0600); said(what, fd >= 0); if (fd >= 0) close(fd); }
        else if (!strcmp(op, "fdread")) { char c; said(what, read(atoi(arg), &c, 1) >= 0); }
        else if (!strcmp(op, "fdwrite")) { said(what, write(atoi(arg), "x", 1) == 1); }
        else if (!strcmp(op, "accept")) { // a listener the parent bound: one connection, one byte back
            int c = accept(atoi(arg), NULL, NULL);
            said(what, c >= 0 && write(c, "y", 1) == 1); if (c >= 0) close(c);
        } else if (!strcmp(op, "fdat")) { // DIRFD:NAME, a file made in a directory passed as a descriptor
            int dfd = atoi(arg); const char *name = strchr(arg, ':') + 1;
            int fd = openat(dfd, name, O_WRONLY | O_CREAT, 0600); said(what, fd >= 0); if (fd >= 0) close(fd);
        } else if (!strcmp(op, "bind")) said(what, unix_socket(arg, 1));
        else if (!strcmp(op, "connect")) said(what, unix_socket(arg, 0));
        else if (!strcmp(op, "tcp")) {
            int s = socket(AF_INET, SOCK_STREAM, 0);
            struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(atoi(arg)), .sin_addr.s_addr = htonl(0x7f000001)};
            said(what, s >= 0 && bind(s, (struct sockaddr *)&a, sizeof a) == 0); if (s >= 0) close(s);
        } else if (!strcmp(op, "hv")) {
            hv_return_t r = hv_vm_create(NULL);
            if (r == HV_SUCCESS) hv_vm_destroy();
            errno = r == HV_SUCCESS ? 0 : EPERM;
            printf("%s: %s (hv_return 0x%x)\n", what, r == HV_SUCCESS ? "ok" : "refused", r);
        } else if (!strcmp(op, "bookmark")) { // HEX@PATH@w|r
            char *hex = strdup(arg), *path = strchr(hex, '@'); *path++ = 0;
            char *mode = strrchr(path, '@'); *mode++ = 0;
            said(what + 0, bookmark(hex, path, *mode == 'w' ? O_WRONLY | O_CREAT : O_RDONLY));
        }
    }
    struct timespec t1;
    clock_gettime(CLOCK_MONOTONIC, &t1);
    printf("main_us: %ld\n", (long)((t1.tv_sec - t0.tv_sec) * 1000000 + (t1.tv_nsec - t0.tv_nsec) / 1000));
    return 0;
}
