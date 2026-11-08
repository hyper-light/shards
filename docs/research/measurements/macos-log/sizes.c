// How long a line os_log keeps whole (PM M374): os_log_with_type of "%{public}s" strings of
// 512 bytes to 64 KiB, each a run of one letter, read back with log show.
#include <os/log.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(int argc, char **argv) {
    const char *tag = argc > 1 ? argv[1] : "m374";
    os_log_t log = os_log_create("shards.m374", tag);
    size_t sizes[] = {512, 1000, 1024, 1025, 2048, 4096, 8192, 16384, 32768, 65536};
    for (size_t i = 0; i < sizeof sizes / sizeof sizes[0]; i++) {
        size_t n = sizes[i];
        char *s = malloc(n + 32);
        int head = snprintf(s, 32, "len%zu:", n);
        memset(s + head, 'a' + (int)(i % 26), n - head);
        s[n] = 0;
        os_log_with_type(log, OS_LOG_TYPE_DEFAULT, "%{public}s", s);
        free(s);
    }
    printf("logged under shards.m374/%s\n", tag);
    return 0;
}
