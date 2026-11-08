/* KVM's supported CPUID as asked on the core this runs on: every leaf KVM_GET_SUPPORTED_CPUID
 * gives, one a line (function, index, flags, eax, ebx, ecx, edx in hex). Run pinned to one
 * core and another (taskset -c N) and diff: on a hybrid Intel host the answer differs by
 * the core asking (PM M258). Build: cc -O2 -o supported supported.c */
#define _GNU_SOURCE
#include <fcntl.h>
#include <sched.h>
#include <linux/kvm.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <unistd.h>

int main(void) {
    int kvm = open("/dev/kvm", O_RDWR | O_CLOEXEC);
    if (kvm < 0) { perror("/dev/kvm"); return 1; }
    int n = 256;
    struct kvm_cpuid2 *c = calloc(1, sizeof(*c) + n * sizeof(struct kvm_cpuid_entry2));
    if (!c) return 1;
    c->nent = n;
    if (ioctl(kvm, KVM_GET_SUPPORTED_CPUID, c) < 0) { perror("KVM_GET_SUPPORTED_CPUID"); return 1; }
    printf("cpu %d\n", sched_getcpu());
    for (unsigned i = 0; i < c->nent; i++) {
        struct kvm_cpuid_entry2 *e = &c->entries[i];
        printf("%08x %02x %x %08x %08x %08x %08x\n", e->function, e->index, e->flags, e->eax, e->ebx, e->ecx, e->edx);
    }
    return 0;
}
