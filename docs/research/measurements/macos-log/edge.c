#include <os/log.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(int argc, char **argv) {
    os_log_t log = os_log_create("shards.m374", argv[1]);
    for (size_t n = 1008; n <= 1024; n++) {
        char *s = malloc(n + 1);
        int head = snprintf(s, 16, "L%zu:", n);
        memset(s + head, 'x', n - head);
        s[n] = 0;
        os_log_with_type(log, OS_LOG_TYPE_DEFAULT, "%{public}s", s);
        free(s);
    }
    return 0;
}
