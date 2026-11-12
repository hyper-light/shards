// softvirt-traps (Windows, x86_64 and arm64): what the primitives cost that a rootless
// software hypervisor would serve a guest with on a Windows host without the Windows
// Hypervisor Platform. Windows has no syscall filter for user processes: a guest's code
// reaches the hypervisor only as a breakpoint or a fault, in its own process (a vectored
// exception handler) or in another (debug events). Guest memory reaches another process
// as views of a section (MapViewOfFile3), whose granularity `views` probes. Each test
// times one round trip per iteration and prints n, p50, p90, p99 and max in nanoseconds
// (QueryPerformanceCounter; its resolution is printed). Results:
// docs/research/platform-measurements.md.
//
//   cl /O2 /W4 windows.c && windows.exe            # every test
//   windows.exe -n 50000 veh debug                 # selected tests

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <intrin.h>
#pragma comment(lib, "advapi32.lib")
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static size_t n = 20000;
static const size_t warm = 1000;
static LARGE_INTEGER freq;

static uint64_t now_ns(void) {
    LARGE_INTEGER c;
    QueryPerformanceCounter(&c);
    return (uint64_t)((double)c.QuadPart * 1e9 / (double)freq.QuadPart);
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static uint64_t pct(const uint64_t *s, size_t len, unsigned p) {
    size_t rank = (p * len + 99) / 100;
    return s[rank ? rank - 1 : 0];
}

static void report(const char *test, const char *variant, uint64_t *s, size_t len) {
    if (len == 0) {
        printf("| %s | %s | 0 | | | | |\n", test, variant);
        fflush(stdout);
        return;
    }
    qsort(s, len, sizeof *s, cmp_u64);
    printf("| %s | %s | %zu | %llu | %llu | %llu | %llu |\n", test, variant, len,
           (unsigned long long)pct(s, len, 50), (unsigned long long)pct(s, len, 90),
           (unsigned long long)pct(s, len, 99), (unsigned long long)s[len - 1]);
    fflush(stdout);
}

static void note(const char *test, const char *variant, const char *what) {
    printf("| %s | %s | %s | | | | |\n", test, variant, what);
    fflush(stdout);
}

static void note_err(const char *test, const char *variant, DWORD err) {
    char what[64];
    snprintf(what, sizeof what, "error %lu", (unsigned long)err);
    note(test, variant, what);
}

#if defined(_M_X64) || defined(__x86_64__)
#define BREAK_LEN 1
static DWORD64 *pc_of(CONTEXT *c) { return &c->Rip; }
#elif defined(_M_ARM64) || defined(__aarch64__)
#define BREAK_LEN 4
static DWORD64 *pc_of(CONTEXT *c) { return &c->Pc; }
#endif
static void advance(CONTEXT *c) { *pc_of(c) += BREAK_LEN; }

static void break_loop(uint64_t *s, size_t total) {
    for (size_t i = 0; i < total; i++) {
        uint64_t t = now_ns();
        __debugbreak();
        uint64_t d = now_ns() - t;
        if (i >= warm)
            s[i - warm] = d;
    }
}

// --- native ---------------------------------------------------------------------------

static void t_native(void) {
    uint64_t *s = calloc(n, sizeof *s);
    for (size_t i = 0; i < n; i++) {
        uint64_t t = now_ns();
        for (int k = 0; k < 64; k++)
            SwitchToThread();
        s[i] = (now_ns() - t) / 64;
    }
    report("native", "NtYieldExecution (each of 64 in a row)", s, n);
    for (size_t i = 0; i < n; i++) {
        uint64_t t = now_ns();
        s[i] = now_ns() - t;
    }
    report("native", "clock read alone", s, n);
}

// --- veh: a breakpoint or a fault served by a vectored handler in-process -------------

static char *g_page;
static SIZE_T g_pagesize;

static LONG CALLBACK on_exception(EXCEPTION_POINTERS *e) {
    DWORD code = e->ExceptionRecord->ExceptionCode;
    if (code == EXCEPTION_BREAKPOINT) {
        advance(e->ContextRecord);
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    if (code == EXCEPTION_ACCESS_VIOLATION &&
        (char *)e->ExceptionRecord->ExceptionInformation[1] >= g_page &&
        (char *)e->ExceptionRecord->ExceptionInformation[1] < g_page + g_pagesize) {
        DWORD old;
        VirtualProtect(g_page, g_pagesize, PAGE_READWRITE, &old);
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    return EXCEPTION_CONTINUE_SEARCH;
}

static void t_veh(void) {
    uint64_t *s = calloc(n, sizeof *s);
    PVOID h = AddVectoredExceptionHandler(1, on_exception);
    break_loop(s, warm + n);
    report("veh", "breakpoint, vectored handler in-process", s, n);
    SYSTEM_INFO si;
    GetSystemInfo(&si);
    g_pagesize = si.dwPageSize;
    g_page = VirtualAlloc(NULL, g_pagesize, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    g_page[0] = 1;
    for (size_t i = 0; i < warm + n; i++) {
        DWORD old;
        VirtualProtect(g_page, g_pagesize, PAGE_NOACCESS, &old);
        uint64_t t = now_ns();
        ((volatile char *)g_page)[0] = 2;
        uint64_t d = now_ns() - t;
        if (i >= warm)
            s[i - warm] = d;
    }
    report("veh", "access violation, handler VirtualProtects the page", s, n);
    RemoveVectoredExceptionHandler(h);
}

// --- debug: a child's breakpoints, served by its parent as a debugger -----------------

static wchar_t self_path[MAX_PATH];

static HANDLE spawn(const wchar_t *mode, DWORD flags, PROCESS_INFORMATION *pi) {
    wchar_t cmd[MAX_PATH + 64];
    _snwprintf(cmd, sizeof cmd / sizeof cmd[0], L"\"%s\" --child %s %zu", self_path, mode, n);
    STARTUPINFOW st;
    memset(&st, 0, sizeof st);
    st.cb = sizeof st;
    st.dwFlags = STARTF_USESTDHANDLES;
    st.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
    st.hStdError = GetStdHandle(STD_ERROR_HANDLE);
    st.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
    fflush(stdout);
    if (!CreateProcessW(self_path, cmd, NULL, NULL, TRUE, flags, NULL, NULL, &st, pi))
        return NULL;
    return pi->hProcess;
}

static void t_debug(void) {
    PROCESS_INFORMATION pi;
    if (!spawn(L"break", DEBUG_ONLY_THIS_PROCESS, &pi)) {
        note_err("debug", "spawn", GetLastError());
        return;
    }
    HANDLE thread = pi.hThread;
    size_t at = 0, past = 0, other = 0;
    DWORD exit_code = 0;
    for (;;) {
        DEBUG_EVENT ev;
        if (!WaitForDebugEventEx(&ev, 60000)) {
            note_err("debug", "wait", GetLastError());
            break;
        }
        DWORD status = DBG_CONTINUE;
        if (ev.dwDebugEventCode == CREATE_PROCESS_DEBUG_EVENT) {
            if (ev.u.CreateProcessInfo.hFile)
                CloseHandle(ev.u.CreateProcessInfo.hFile);
        } else if (ev.dwDebugEventCode == LOAD_DLL_DEBUG_EVENT) {
            if (ev.u.LoadDll.hFile)
                CloseHandle(ev.u.LoadDll.hFile);
        } else if (ev.dwDebugEventCode == EXCEPTION_DEBUG_EVENT) {
            EXCEPTION_RECORD *er = &ev.u.Exception.ExceptionRecord;
            if (er->ExceptionCode == EXCEPTION_BREAKPOINT) {
                // The loader's own breakpoint comes first. A debugger may find the thread at
                // the break instruction or already past it: step over it only when at it,
                // and count which one Windows gives.
                HANDLE th = ev.dwThreadId == pi.dwThreadId
                                ? thread
                                : OpenThread(THREAD_GET_CONTEXT | THREAD_SET_CONTEXT, FALSE,
                                             ev.dwThreadId);
                CONTEXT c;
                memset(&c, 0, sizeof c);
                c.ContextFlags = CONTEXT_CONTROL | CONTEXT_INTEGER;
                GetThreadContext(th, &c);
                if (*pc_of(&c) == (DWORD64)(uintptr_t)er->ExceptionAddress) {
                    advance(&c);
                    SetThreadContext(th, &c);
                    at++;
                } else {
                    past++;
                }
                if (th != thread)
                    CloseHandle(th);
            } else {
                if (other++ == 0) {
                    char what[96];
                    snprintf(what, sizeof what, "exception 0x%08lx at %p, %s chance",
                             (unsigned long)er->ExceptionCode, er->ExceptionAddress,
                             ev.u.Exception.dwFirstChance ? "first" : "last");
                    note("debug", "child", what);
                }
                status = DBG_EXCEPTION_NOT_HANDLED;
            }
        } else if (ev.dwDebugEventCode == EXIT_PROCESS_DEBUG_EVENT) {
            exit_code = ev.u.ExitProcess.dwExitCode;
            ContinueDebugEvent(ev.dwProcessId, ev.dwThreadId, DBG_CONTINUE);
            break;
        }
        ContinueDebugEvent(ev.dwProcessId, ev.dwThreadId, status);
    }
    char what[128];
    snprintf(what, sizeof what,
             "%zu with the PC at the instruction, %zu past it; %zu other exceptions; exit %lu",
             at, past, other, (unsigned long)exit_code);
    note("debug", "breakpoints served", what);
    WaitForSingleObject(pi.hProcess, INFINITE);
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
}

// --- views: guest memory mapped into another process ----------------------------------

typedef PVOID(WINAPI *map3_t)(HANDLE, HANDLE, PVOID, ULONG64, SIZE_T, ULONG, ULONG, PVOID,
                              ULONG);
typedef BOOL(WINAPI *unmap2_t)(HANDLE, PVOID, ULONG);
typedef PVOID(WINAPI *alloc2_t)(HANDLE, PVOID, SIZE_T, ULONG, ULONG, PVOID, ULONG);

static void t_views(void) {
    HMODULE kb = GetModuleHandleW(L"kernelbase.dll");
    map3_t map3 = (map3_t)(void *)GetProcAddress(kb, "MapViewOfFile3");
    unmap2_t unmap2 = (unmap2_t)(void *)GetProcAddress(kb, "UnmapViewOfFile2");
    alloc2_t alloc2 = (alloc2_t)(void *)GetProcAddress(kb, "VirtualAlloc2");
    if (!map3 || !unmap2 || !alloc2) {
        note("views", "MapViewOfFile3", "unavailable");
        return;
    }
    PROCESS_INFORMATION pi;
    if (!spawn(L"idle", 0, &pi)) {
        note_err("views", "spawn", GetLastError());
        return;
    }
    SIZE_T unit = 64 << 10, pages = 512;
    HANDLE section = CreateFileMappingW(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0,
                                        (DWORD)(pages * unit), NULL);
    uint64_t *map = calloc(n, sizeof *map), *unmap = calloc(n, sizeof *unmap);
    size_t done = 0;
    for (size_t i = 0; i < n; i++) {
        uint64_t t = now_ns();
        PVOID at = map3(section, pi.hProcess, NULL, (ULONG64)((i % pages) * unit), unit, 0,
                        PAGE_READWRITE, NULL, 0);
        map[i] = now_ns() - t;
        if (!at) {
            note_err("views", "map a 64 KiB view into the child", GetLastError());
            break;
        }
        t = now_ns();
        unmap2(pi.hProcess, at, 0);
        unmap[i] = now_ns() - t;
        done++;
    }
    report("views", "map a 64 KiB view into the child", map, done);
    report("views", "unmap it", unmap, done);

    // A guest's 4 KiB pages: placeholders split to one page each, each replaced by a
    // 4 KiB view, one at a 4 KiB (not 64 KiB) offset of the section, one at offset 0.
    char *ph = alloc2(pi.hProcess, NULL, 2 * unit, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                      PAGE_NOACCESS, NULL, 0);
    if (!ph) {
        note_err("views", "placeholder", GetLastError());
    } else {
        BOOL split = VirtualFreeEx(pi.hProcess, ph, 4096, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) &&
                     VirtualFreeEx(pi.hProcess, ph + 4096, 4096,
                                   MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
        if (!split) {
            note_err("views", "split a placeholder at 4 KiB", GetLastError());
        } else {
            note("views", "split a placeholder at 4 KiB", "ok");
            PVOID v = map3(section, pi.hProcess, ph, 4096, 4096, MEM_REPLACE_PLACEHOLDER,
                           PAGE_READWRITE, NULL, 0);
            if (v)
                note("views", "a 4 KiB view at a 4 KiB offset", "ok");
            else
                note_err("views", "a 4 KiB view at a 4 KiB offset", GetLastError());
            v = map3(section, pi.hProcess, ph + 4096, 0, 4096, MEM_REPLACE_PLACEHOLDER,
                     PAGE_READWRITE, NULL, 0);
            if (v)
                note("views", "a 4 KiB view at offset 0", "ok");
            else
                note_err("views", "a 4 KiB view at offset 0", GetLastError());
        }
    }
    TerminateProcess(pi.hProcess, 0);
    WaitForSingleObject(pi.hProcess, INFINITE);
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    CloseHandle(section);
}

// --- jit: what a translator pays to publish code -------------------------------------

static void t_jit(void) {
    SIZE_T len = 64 << 10;
    uint32_t *code = VirtualAlloc(NULL, len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    uint64_t *s = calloc(n, sizeof *s);
    for (size_t i = 0; i < warm + n; i++) {
        DWORD old;
        uint64_t t = now_ns();
        VirtualProtect(code, 4096, PAGE_READWRITE, &old);
        VirtualProtect(code, 4096, PAGE_EXECUTE_READ, &old);
        uint64_t d = now_ns() - t;
        if (i >= warm)
            s[i - warm] = d;
    }
    report("jit", "VirtualProtect read-write, then execute-read", s, n);
    for (size_t i = 0; i < n; i++) {
        uint64_t t = now_ns();
        FlushInstructionCache(GetCurrentProcess(), code, 8);
        s[i] = now_ns() - t;
    }
    report("jit", "FlushInstructionCache of 8 bytes", s, n);
}

static int child_main(const wchar_t *mode) {
    if (!wcscmp(mode, L"break")) {
        uint64_t *s = calloc(n, sizeof *s);
        break_loop(s, warm + n);
        report("debug", "breakpoint, debug event to the parent", s, n);
        return 0;
    }
    if (!wcscmp(mode, L"idle")) {
        Sleep(120000);
        return 0;
    }
    return 2;
}

struct test {
    const char *name;
    void (*run)(void);
};

static const struct test tests[] = {
    {"native", t_native}, {"veh", t_veh}, {"debug", t_debug}, {"views", t_views}, {"jit", t_jit},
};

int wmain(int argc, wchar_t **argv) {
    QueryPerformanceFrequency(&freq);
    GetModuleFileNameW(NULL, self_path, MAX_PATH);
    if (argc == 4 && !wcscmp(argv[1], L"--child")) {
        n = (size_t)_wtoi64(argv[3]);
        return child_main(argv[2]);
    }
    int first = 1;
    if (argc > 2 && !wcscmp(argv[1], L"-n")) {
        n = (size_t)_wtoi64(argv[2]);
        first = 3;
    }
    if (n == 0)
        return 2;
    SYSTEM_INFO si;
    GetSystemInfo(&si);
    printf("- QPC frequency: %lld Hz (%.1f ns a tick)\n", (long long)freq.QuadPart,
           1e9 / (double)freq.QuadPart);
    printf("- processors: %lu; page %lu bytes; allocation granularity %lu bytes\n",
           (unsigned long)si.dwNumberOfProcessors, (unsigned long)si.dwPageSize,
           (unsigned long)si.dwAllocationGranularity);
    wchar_t cpu[128];
    DWORD cpu_len = sizeof cpu;
    if (RegGetValueW(HKEY_LOCAL_MACHINE, L"HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0",
                     L"ProcessorNameString", RRF_RT_REG_SZ, NULL, cpu, &cpu_len) ==
        ERROR_SUCCESS)
        printf("- cpu: %ls\n", cpu);
    printf("\n| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |\n");
    printf("|---|---|---|---|---|---|---|\n");
    fflush(stdout);
    for (size_t t = 0; t < sizeof tests / sizeof tests[0]; t++) {
        int chosen = first >= argc;
        for (int a = first; a < argc; a++) {
            char name[32];
            snprintf(name, sizeof name, "%ls", argv[a]);
            chosen |= !strcmp(name, tests[t].name);
        }
        if (chosen)
            tests[t].run();
    }
    return 0;
}
