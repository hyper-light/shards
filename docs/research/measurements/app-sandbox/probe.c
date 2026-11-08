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
#include <sys/stat.h>
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

// A directory bookmark, hex-encoded, resolved and its scope started; a file made in DIR;
// the scope stopped (CFURLStopAccessingSecurityScopedResource); another file made in DIR.
static void relinquish(const char *hex, const char *dir) {
    size_t n = strlen(hex) / 2;
    UInt8 *b = malloc(n);
    for (size_t i = 0; i < n; i++) sscanf(hex + 2 * i, "%2hhx", &b[i]);
    CFDataRef d = CFDataCreate(NULL, b, (CFIndex)n);
    Boolean stale = false;
    CFURLRef u = CFURLCreateByResolvingBookmarkData(NULL, d, 0, NULL, NULL, &stale, NULL);
    if (!u) { errno = EPERM; said("relinquish resolve", 0); return; }
    Boolean started = CFURLStartAccessingSecurityScopedResource(u);
    char path[1200];
    snprintf(path, sizeof path, "%s/before", dir);
    int fd = open(path, O_WRONLY | O_CREAT, 0600);
    printf("  (scope started %d)\n", started);
    said("relinquish write before stop", fd >= 0);
    if (fd >= 0) close(fd);
    CFURLStopAccessingSecurityScopedResource(u);
    snprintf(path, sizeof path, "%s/after", dir);
    fd = open(path, O_WRONLY | O_CREAT, 0600);
    said("relinquish write after stop", fd >= 0);
    if (fd >= 0) close(fd);
    snprintf(path, sizeof path, "%s/before", dir);
    fd = open(path, O_RDONLY);
    said("relinquish read after stop", fd >= 0);
    if (fd >= 0) close(fd);
}

// A directory bookmark, hex-encoded, resolved and its scope started; then, once the
// parent has renamed the directory to MOVED (it waits on stdin for this tool's word),
// a file made at MOVED, and one made at DIR again.
static void renamed(const char *hex, const char *dir, const char *moved) {
    size_t n = strlen(hex) / 2;
    UInt8 *b = malloc(n);
    for (size_t i = 0; i < n; i++) sscanf(hex + 2 * i, "%2hhx", &b[i]);
    CFDataRef d = CFDataCreate(NULL, b, (CFIndex)n);
    Boolean stale = false;
    CFURLRef u = CFURLCreateByResolvingBookmarkData(NULL, d, 0, NULL, NULL, &stale, NULL);
    if (!u) { errno = EPERM; said("renamed resolve", 0); return; }
    Boolean started = CFURLStartAccessingSecurityScopedResource(u);
    char path[1200];
    snprintf(path, sizeof path, "%s/before", dir);
    int fd = open(path, O_WRONLY | O_CREAT, 0600);
    printf("  (scope started %d)\n", started);
    said("renamed write before the rename", fd >= 0);
    if (fd >= 0) close(fd);
    printf("ready\n");
    fflush(stdout);
    char c;
    if (read(0, &c, 1) != 1) return;
    snprintf(path, sizeof path, "%s/after", moved);
    fd = open(path, O_WRONLY | O_CREAT, 0600);
    said("renamed write at the new path", fd >= 0);
    if (fd >= 0) close(fd);
    snprintf(path, sizeof path, "%s/before", moved);
    fd = open(path, O_WRONLY);
    said("renamed rewrite of a file at the new path", fd >= 0);
    if (fd >= 0) close(fd);
    said("renamed mkdir at the old path", mkdir(dir, 0700) == 0);
}

// What a VM taken over after its template is renamed out of its grant could still try
// (PM M165): a directory descriptor opened before the rename, used after it to make,
// rewrite and plant; the same bookmark resolved again, which tracks the directory, not
// its path; and a symlink planted in the granted directory before the rename.
static void reresolve(const char *hex, const char *dir, const char *moved) {
    size_t n = strlen(hex) / 2;
    UInt8 *b = malloc(n);
    for (size_t i = 0; i < n; i++) sscanf(hex + 2 * i, "%2hhx", &b[i]);
    CFDataRef d = CFDataCreate(NULL, b, (CFIndex)n);
    Boolean stale = false;
    CFURLRef u = CFURLCreateByResolvingBookmarkData(NULL, d, 0, NULL, NULL, &stale, NULL);
    if (!u) { errno = EPERM; said("reresolve resolve", 0); return; }
    Boolean started = CFURLStartAccessingSecurityScopedResource(u);
    printf("  (scope started %d)\n", started);
    char path[1200];
    snprintf(path, sizeof path, "%s/before", dir);
    int fd = open(path, O_WRONLY | O_CREAT, 0600);
    said("reresolve write before the rename", fd >= 0);
    if (fd >= 0) close(fd);
    int dfd = open(dir, O_RDONLY | O_DIRECTORY);
    said("reresolve directory descriptor before the rename", dfd >= 0);
    snprintf(path, sizeof path, "%s/planted", dir);
    said("reresolve symlink planted before the rename", symlink("/etc/hosts", path) == 0);
    printf("ready\n");
    fflush(stdout);
    char c;
    if (read(0, &c, 1) != 1) return;
    if (dfd >= 0) {
        fd = openat(dfd, "viafd", O_WRONLY | O_CREAT, 0600);
        said("reresolve make through the descriptor after the rename", fd >= 0);
        if (fd >= 0) close(fd);
        fd = openat(dfd, "before", O_WRONLY);
        said("reresolve rewrite through the descriptor after the rename", fd >= 0);
        if (fd >= 0) close(fd);
        said("reresolve symlink through the descriptor after the rename", symlinkat("/etc/hosts", dfd, "planted2") == 0);
    }
    Boolean stale2 = false;
    CFURLRef u2 = CFURLCreateByResolvingBookmarkData(NULL, d, 0, NULL, NULL, &stale2, NULL);
    if (!u2) { errno = EPERM; said("reresolve resolve again", 0); return; }
    char again[1200];
    if (!CFURLGetFileSystemRepresentation(u2, 1, (UInt8 *)again, sizeof again)) again[0] = 0;
    printf("  (resolved again to %s, stale %d)\n", again, stale2);
    Boolean started2 = CFURLStartAccessingSecurityScopedResource(u2);
    printf("  (scope started again %d)\n", started2);
    snprintf(path, sizeof path, "%s/after", moved);
    fd = open(path, O_WRONLY | O_CREAT, 0600);
    said("reresolve write at the new path after resolving again", fd >= 0);
    if (fd >= 0) close(fd);
    snprintf(path, sizeof path, "%s/before", moved);
    fd = open(path, O_WRONLY);
    said("reresolve rewrite at the new path after resolving again", fd >= 0);
    if (fd >= 0) close(fd);
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
        } else if (!strcmp(op, "renamed")) { // HEX@DIR@MOVED
            char *hex = strdup(arg), *dir = strchr(hex, '@'); *dir++ = 0;
            char *moved = strchr(dir, '@'); *moved++ = 0;
            renamed(hex, dir, moved);
        } else if (!strcmp(op, "reresolve")) { // HEX@DIR@MOVED
            char *hex = strdup(arg), *dir = strchr(hex, '@'); *dir++ = 0;
            char *moved = strchr(dir, '@'); *moved++ = 0;
            reresolve(hex, dir, moved);
        } else if (!strcmp(op, "relinquish")) { // HEX@DIR
            char *hex = strdup(arg), *dir = strchr(hex, '@'); *dir++ = 0;
            relinquish(hex, dir);
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
