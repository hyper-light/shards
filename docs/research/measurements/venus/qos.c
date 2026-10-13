// E6: a library that raises the process it is loaded into to QoS user-interactive, the
// class shards' vCPU threads run at (crates/vmm/src/platform/macos_thread.rs, M8, M10),
// before its main runs: so that the host's side is scheduled as the guest's is on a busy
// Mac. A spawn may only lower a process's class (pthread/spawn.h), so it is loaded:
//   cc -dynamiclib -o qos.dylib qos.c
//   DYLD_INSERT_LIBRARIES=qos.dylib COMMAND [ARG...]
#include <pthread/qos.h>
#include <stdio.h>

__attribute__((constructor)) static void raise_qos(void) {
    int rc = pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
    if (rc != 0) fprintf(stderr, "qos: pthread_set_qos_class_self_np: error %d\n", rc);
}
