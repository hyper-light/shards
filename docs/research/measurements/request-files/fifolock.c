// M254 (docs/research/platform-measurements.md): flock(2) on a FIFO's descriptor (D185):
// taken? exclusive across two opens? released at close? And macOS's O_EXLOCK.
//
//   fifolock DIR
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <unistd.h>
int main(int argc, char **argv) {
    char p[512]; snprintf(p, sizeof p, "%s/f", argv[1]);
    unlink(p); mkfifo(p, 0600);
    int r = open(p, O_RDONLY | O_NONBLOCK);
    int a = open(p, O_WRONLY), b = open(p, O_WRONLY);
    int x = flock(a, LOCK_EX | LOCK_NB); int ex = errno;
    int y = flock(b, LOCK_EX | LOCK_NB); int ey = errno;
    close(a);
    int z = flock(b, LOCK_EX | LOCK_NB); int ez = errno;
    printf("first %d (%s), second while held %d (%s), second after close %d (%s)\n", x, x ? strerror(ex) : "ok", y, y ? strerror(ey) : "ok", z, z ? strerror(ez) : "ok");
#ifdef O_EXLOCK
    int c = open(p, O_WRONLY | O_EXLOCK | O_NONBLOCK); int ec = errno;
    printf("O_EXLOCK open while b holds it: %d (%s)\n", c, c < 0 ? strerror(ec) : "opened");
#endif
    close(r); unlink(p);
    return 0;
}
