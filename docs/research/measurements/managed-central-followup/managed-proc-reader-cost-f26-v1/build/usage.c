#include <stdint.h>
#include <sys/resource.h>
int reader_usage(int64_t *values) {
    struct rusage usage;
    int error = getrusage(RUSAGE_SELF, &usage);
    if (error) return error;
    values[0] = usage.ru_utime.tv_sec * 1000000LL + usage.ru_utime.tv_usec;
    values[1] = usage.ru_stime.tv_sec * 1000000LL + usage.ru_stime.tv_usec;
    values[2] = usage.ru_minflt;
    values[3] = usage.ru_majflt;
    return 0;
}
