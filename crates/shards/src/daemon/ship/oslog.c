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

// Reading back what the log kept of this process's entries, for the oslog driver to learn
// how long a line os_log keeps whole (oslog.rs `whole`): OSLogStore, the log store of the
// current process (OSLogStoreCurrentProcessIdentifier, macOS 12, which needs no
// entitlement: Apple's documentation, "OSLogStore", storeWithScope:error:), its entries of
// one category enumerated, each one's composedMessage read. OSLog and the Objective-C
// runtime are bound here as first called (dlopen), so that `shards` links neither at
// launch (CLAUDE.md, PM M113).
#include <dlfcn.h>
#include <string.h>
#include <stdio.h>

typedef void *shards_id;
typedef void *shards_sel;

static shards_id (*get_class)(const char *);
static shards_sel (*sel)(const char *);
static void *msg_send;
static void *(*pool_push)(void);
static void (*pool_pop)(void *);

static int bind_objc(void) {
    if (msg_send) {
        return 1;
    }
    void *objc = dlopen("/usr/lib/libobjc.A.dylib", RTLD_LAZY);
    void *oslog = dlopen("/System/Library/Frameworks/OSLog.framework/OSLog", RTLD_LAZY);
    if (!objc || !oslog) {
        return 0;
    }
    get_class = (shards_id (*)(const char *))dlsym(objc, "objc_getClass");
    sel = (shards_sel (*)(const char *))dlsym(objc, "sel_registerName");
    pool_push = (void *(*)(void))dlsym(objc, "objc_autoreleasePoolPush");
    pool_pop = (void (*)(void *))dlsym(objc, "objc_autoreleasePoolPop");
    void *send = dlsym(objc, "objc_msgSend");
    if (!get_class || !sel || !pool_push || !pool_pop || !send) {
        return 0;
    }
    msg_send = send;
    return 1;
}

// The composed message of the last entry of subsystem `shards` and `category` in this
// process's log store that begins with `prefix`, into `buf` (NUL-terminated, at most `cap`
// bytes with its NUL): its length; -1 where none is found; -2 where the store cannot be read.
long shards_os_log_read_back(const char *category, const char *prefix, char *buf, long cap) {
    if (!bind_objc() || cap < 1) {
        return -2;
    }
    shards_id (*send0)(shards_id, shards_sel) = (shards_id (*)(shards_id, shards_sel))msg_send;
    shards_id (*send_str)(shards_id, shards_sel, const char *) =
        (shards_id (*)(shards_id, shards_sel, const char *))msg_send;
    shards_id (*send_store)(shards_id, shards_sel, long, shards_id *) =
        (shards_id (*)(shards_id, shards_sel, long, shards_id *))msg_send;
    shards_id (*send_enum)(shards_id, shards_sel, unsigned long, shards_id, shards_id, shards_id *) =
        (shards_id (*)(shards_id, shards_sel, unsigned long, shards_id, shards_id, shards_id *))msg_send;
    const char *(*send_utf8)(shards_id, shards_sel) = (const char *(*)(shards_id, shards_sel))msg_send;
    shards_id (*send_id)(shards_id, shards_sel, shards_id) = (shards_id (*)(shards_id, shards_sel, shards_id))msg_send;
    shards_id store_class = get_class("OSLogStore");
    shards_id string_class = get_class("NSString");
    shards_id predicate_class = get_class("NSPredicate");
    if (!store_class || !string_class || !predicate_class) {
        return -2;
    }
    void *pool = pool_push();
    long found = -2;
    shards_id error = 0;
    // OSLogStoreCurrentProcessIdentifier.
    shards_id store = send_store(store_class, sel("storeWithScope:error:"), 1, &error);
    char format[256];
    int n = snprintf(format, sizeof format, "subsystem == 'shards' AND category == '%s'", category);
    if (store && n > 0 && (size_t)n < sizeof format) {
        shards_id text = send_str(string_class, sel("stringWithUTF8String:"), format);
        shards_id predicate = text ? send_id(predicate_class, sel("predicateWithFormat:"), text) : 0;
        shards_id entries = predicate
            ? send_enum(store, sel("entriesEnumeratorWithOptions:position:predicate:error:"), 0, 0, predicate, &error)
            : 0;
        if (entries) {
            found = -1;
            size_t want = strlen(prefix);
            shards_id entry;
            while ((entry = send0(entries, sel("nextObject")))) {
                shards_id message = send0(entry, sel("composedMessage"));
                const char *utf8 = message ? send_utf8(message, sel("UTF8String")) : 0;
                if (!utf8 || strncmp(utf8, prefix, want) != 0) {
                    continue;
                }
                size_t len = strlen(utf8);
                size_t kept = len < (size_t)(cap - 1) ? len : (size_t)(cap - 1);
                memcpy(buf, utf8, kept);
                buf[kept] = 0;
                found = (long)len;
            }
        }
    }
    pool_pop(pool);
    return found;
}
