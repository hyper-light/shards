/*
 * The render server's log (D201), in place of its own (server/render_common.c), which
 * opens syslog: the server runs as a thread of the GPU's VM process, which says what it
 * logs on its standard error, each message one write, as the process's own are.
 */

#include <stdarg.h>
#include <stdio.h>
#include <unistd.h>

void render_log_init(void) {}

void render_log(const char *fmt, ...)
{
    char line[512];
    int n = snprintf(line, sizeof(line), "virtio-gpu: render server: ");
    va_list va;
    va_start(va, fmt);
    int said = vsnprintf(line + n, sizeof(line) - (size_t)n - 1, fmt, va);
    va_end(va);
    if (said < 0)
        return;
    n += said;
    if ((size_t)n > sizeof(line) - 2)
        n = (int)sizeof(line) - 2;
    line[n++] = '\n';
    (void)!write(STDERR_FILENO, line, (size_t)n);
}
