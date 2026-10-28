// os_log for the oslog driver (daemon/ship/oslog.rs, D199): its macros, which only a C
// compiler expands (os/log.h's os_log_with_type, through clang's __builtin_os_log_format),
// as functions Rust calls. libSystem's own, loaded with every process: no launch cost.
#include <os/log.h>
#include <os/object.h>
#include <stdint.h>

os_log_t shards_os_log_create(const char *subsystem, const char *category) {
    return os_log_create(subsystem, category);
}

// One entry of `line`, whole and public: what a run wrote is no secret of shards' to redact.
void shards_os_log(os_log_t log, uint8_t type, const char *line) {
    os_log_with_type(log, (os_log_type_t)type, "%{public}s", line);
}

// os_log_create's log is returned retained (OS_OBJECT_RETURNS_RETAINED): let go of here.
void shards_os_log_release(os_log_t log) {
    os_release(log);
}
