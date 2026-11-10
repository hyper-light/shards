#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <netdb.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

/* Test-only interposition: the real worker stays inside libc's blocking call until
 * its local controller releases it. No resolver configuration or network is changed. */
static void held(char kind) {
    const char *path = getenv("SHARDS_CANCEL_FIXTURE_SOCKET");
    if (!path) return;
    int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0) _exit(70);
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    if (strlen(path) >= sizeof(addr.sun_path)) _exit(71);
    strcpy(addr.sun_path, path);
    int (*real_connect)(int, const struct sockaddr *, socklen_t) = dlsym(RTLD_NEXT, "connect");
    if (!real_connect || real_connect(fd, (const struct sockaddr *)&addr, sizeof(addr))) _exit(72);
    if (write(fd, &kind, 1) != 1) _exit(73);
    char release;
    while (read(fd, &release, 1) < 0 && errno == EINTR) {}
    close(fd);
}

int getaddrinfo(const char *node, const char *service, const struct addrinfo *hints, struct addrinfo **res) {
    if (node && !strcmp(node, "shards-cancel.invalid")) {
        held('r');
        return EAI_AGAIN;
    }
    int (*real_getaddrinfo)(const char *, const char *, const struct addrinfo *, struct addrinfo **) = dlsym(RTLD_NEXT, "getaddrinfo");
    if (!real_getaddrinfo) return EAI_SYSTEM;
    return real_getaddrinfo(node, service, hints, res);
}

int connect(int fd, const struct sockaddr *addr, socklen_t size) {
    if (addr && addr->sa_family == AF_INET && size >= sizeof(struct sockaddr_in)) {
        const struct sockaddr_in *ip = (const struct sockaddr_in *)addr;
        if (ip->sin_addr.s_addr == htonl(0xc0000201) && ip->sin_port == htons(9)) {
            held('d');
            errno = ECONNREFUSED;
            return -1;
        }
    }
    int (*real_connect)(int, const struct sockaddr *, socklen_t) = dlsym(RTLD_NEXT, "connect");
    if (!real_connect) { errno = ENOSYS; return -1; }
    return real_connect(fd, addr, size);
}
