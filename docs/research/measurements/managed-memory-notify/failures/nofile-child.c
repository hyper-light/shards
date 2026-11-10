/* ROOT-only fresh-process helper. Never call this from a fork preexec callback.
 * See README.md/source identities. No parent/host/global resource mutation. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <unistd.h>

static int failure(const char *operation, int error)
{
    (void)fprintf(stderr, "launcher_child phase=error operation=%s errno=%d\n",
                  operation, error);
    return EXIT_FAILURE;
}

static int decimal(const char *text, uintmax_t *value)
{
    uintmax_t result = 0;
    if (!text || !*text)
        return -1;
    for (const unsigned char *p = (const unsigned char *)text; *p; ++p) {
        if (*p < '0' || *p > '9')
            return -1;
        unsigned digit = *p - '0';
        if (result > (UINTMAX_MAX - digit) / 10)
            return -1;
        result = result * 10 + digit;
    }
    if (!result)
        return -1;
    *value = result;
    return 0;
}

static int stdio_only(void)
{
    /* Popen has already exec'd this helper. Its exec-error pipe is gone.
     * No Python/runtime callback or parent descriptor-table change occurs here. */
#ifdef SYS_close_range
    if (syscall(SYS_close_range, STDERR_FILENO + 1U, UINT_MAX, 0U) < 0)
        return failure("close_range", errno);
#else
    return failure("close_range_unavailable", ENOSYS);
#endif
    for (int fd = STDIN_FILENO; fd <= STDERR_FILENO; ++fd) {
        int flags = fcntl(fd, F_GETFD);
        if (flags < 0)
            return failure("stdio_getfd", errno);
        if (flags & FD_CLOEXEC)
            return failure("stdio_cloexec", EINVAL);
    }
    /* Observe BEFORE lowering the limit; this transient directory fd must not
     * consume an admitted target slot or turn the intended failure into opendir. */
    DIR *directory = opendir("/proc/self/fd");
    if (!directory)
        return failure("fd_observer_open", errno);
    int observer = dirfd(directory);
    unsigned application = 0;
    int error = 0;
    errno = 0;
    for (struct dirent *entry; (entry = readdir(directory)) != NULL;) {
        if (entry->d_name[0] == '.')
            continue;
        char *end = NULL;
        errno = 0;
        unsigned long number = strtoul(entry->d_name, &end, 10);
        if (errno || !end || *end || number > INT_MAX) {
            error = EINVAL;
            break;
        }
        if ((int)number != observer) {
            if (number > STDERR_FILENO || application == STDERR_FILENO + 1U) {
                error = EINVAL;
                break;
            }
            ++application;
        }
        errno = 0;
    }
    if (errno && !error)
        error = errno;
    if (closedir(directory) < 0 && !error)
        error = errno;
    if (error || application != STDERR_FILENO + 1U)
        return failure("fd_inheritance_snapshot", error ? error : EINVAL);
    if (printf("launcher_child phase=stdio pid=%jd observer_fd=%d observer_closed=true application_fds=%u\n",
               (intmax_t)getpid(), observer, application) < 0)
        return failure("stdio_report", errno);
    return EXIT_SUCCESS;
}

int main(int argc, char **argv)
{
    /* SOFT BINARY NEW_DIRECTORY ORIGINAL_DEADLINE_NS WRITES READ_BYTES */
    if (argc != 7 || argv[2][0] != '/' || argv[3][0] != '/')
        return failure("arguments", EINVAL);
    uintmax_t requested;
    if (decimal(argv[1], &requested) < 0 || (uintmax_t)(rlim_t)requested != requested)
        return failure("nofile_argument", EINVAL);
    for (int index = 4; index < argc; ++index) {
        uintmax_t ignored;
        if (decimal(argv[index], &ignored) < 0)
            return failure("native_argument", EINVAL);
    }
    if (setvbuf(stdout, NULL, _IONBF, 0) || setvbuf(stderr, NULL, _IONBF, 0))
        return failure("unbuffered_reports", EIO);
    if (stdio_only() != EXIT_SUCCESS)
        return EXIT_FAILURE;
    struct rlimit before;
    if (getrlimit(RLIMIT_NOFILE, &before) < 0)
        return failure("getrlimit_before", errno);
    if (requested > before.rlim_cur || requested > before.rlim_max)
        return failure("nofile_increase_refused", EINVAL);
    struct rlimit lowered = before;
    lowered.rlim_cur = (rlim_t)requested;
    if (setrlimit(RLIMIT_NOFILE, &lowered) < 0)
        return failure("setrlimit_child_soft", errno);
    struct rlimit after;
    if (getrlimit(RLIMIT_NOFILE, &after) < 0)
        return failure("getrlimit_after", errno);
    if (after.rlim_cur != lowered.rlim_cur || after.rlim_max != before.rlim_max)
        return failure("limit_readback", EINVAL);
    if (printf("launcher_child phase=limit pid=%jd before_soft=%ju before_hard=%ju after_soft=%ju after_hard=%ju\n",
               (intmax_t)getpid(), (uintmax_t)before.rlim_cur, (uintmax_t)before.rlim_max,
               (uintmax_t)after.rlim_cur, (uintmax_t)after.rlim_max) < 0)
        return failure("limit_report", errno);
    char *native[] = {argv[2], argv[3], argv[4], argv[5], argv[6], NULL};
    execv(argv[2], native);
    return failure("exec_native", errno);
}
