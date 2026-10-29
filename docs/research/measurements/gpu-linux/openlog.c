// Which files a program opens, stats or lists: each path, and how, written to the file
// OPENLOG names, one line a call (LD_PRELOAD). For finding what a GPU driver reads
// beyond its libraries, which LD_DEBUG=files names.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include <dirent.h>

static int logfd = -2;

static void say(const char *what, const char *path, int flags, long result) {
    if (logfd == -2) {
        const char *p = getenv("OPENLOG");
        logfd = p ? ((int (*)(const char *, int, ...))dlsym(RTLD_NEXT, "open"))(p, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644) : -1;
    }
    if (logfd < 0 || !path) return;
    char line[600];
    int n = snprintf(line, sizeof line, "%s %s %#x %ld\n", what, path, flags, result);
    if (n > 0) (void)!write(logfd, line, (size_t)(n < (int)sizeof line ? n : (int)sizeof line - 1));
}

#define FORWARD_OPEN(name)                                                         \
    int name(const char *path, int flags, ...) {                                   \
        static int (*real)(const char *, int, ...);                                \
        if (!real) real = (int (*)(const char *, int, ...))dlsym(RTLD_NEXT, #name); \
        va_list va; va_start(va, flags); mode_t mode = va_arg(va, int); va_end(va); \
        int r = real(path, flags, mode);                                           \
        say(#name, path, flags, r);                                                \
        return r;                                                                  \
    }
FORWARD_OPEN(open)
FORWARD_OPEN(open64)

#define FORWARD_OPENAT(name)                                                            \
    int name(int dir, const char *path, int flags, ...) {                               \
        static int (*real)(int, const char *, int, ...);                                \
        if (!real) real = (int (*)(int, const char *, int, ...))dlsym(RTLD_NEXT, #name); \
        va_list va; va_start(va, flags); mode_t mode = va_arg(va, int); va_end(va);      \
        int r = real(dir, path, flags, mode);                                           \
        say(#name, path, flags, r);                                                     \
        return r;                                                                       \
    }
FORWARD_OPENAT(openat)
FORWARD_OPENAT(openat64)

FILE *fopen(const char *path, const char *mode) {
    static FILE *(*real)(const char *, const char *);
    if (!real) real = (FILE * (*)(const char *, const char *)) dlsym(RTLD_NEXT, "fopen");
    FILE *f = real(path, mode);
    say("fopen", path, mode && mode[0] == 'r' ? 0 : 1, f ? 0 : -1);
    return f;
}
FILE *fopen64(const char *path, const char *mode) {
    static FILE *(*real)(const char *, const char *);
    if (!real) real = (FILE * (*)(const char *, const char *)) dlsym(RTLD_NEXT, "fopen64");
    FILE *f = real(path, mode);
    say("fopen64", path, mode && mode[0] == 'r' ? 0 : 1, f ? 0 : -1);
    return f;
}
DIR *opendir(const char *path) {
    static DIR *(*real)(const char *);
    if (!real) real = (DIR * (*)(const char *)) dlsym(RTLD_NEXT, "opendir");
    DIR *d = real(path);
    say("opendir", path, 0, d ? 0 : -1);
    return d;
}
int access(const char *path, int mode) {
    static int (*real)(const char *, int);
    if (!real) real = (int (*)(const char *, int))dlsym(RTLD_NEXT, "access");
    int r = real(path, mode);
    say("access", path, mode, r);
    return r;
}
int stat(const char *path, struct stat *st) {
    static int (*real)(const char *, struct stat *);
    if (!real) real = (int (*)(const char *, struct stat *))dlsym(RTLD_NEXT, "stat");
    int r = real(path, st);
    say("stat", path, 0, r);
    return r;
}
ssize_t readlink(const char *path, char *buf, size_t len) {
    static ssize_t (*real)(const char *, char *, size_t);
    if (!real) real = (ssize_t (*)(const char *, char *, size_t))dlsym(RTLD_NEXT, "readlink");
    ssize_t r = real(path, buf, len);
    say("readlink", path, 0, r);
    return r;
}
int symlink(const char *target, const char *path) {
    static int (*real)(const char *, const char *);
    if (!real) real = (int (*)(const char *, const char *))dlsym(RTLD_NEXT, "symlink");
    int r = real(target, path);
    say("symlink", path, 0, r);
    say("symlink-target", target, 0, r);
    return r;
}
