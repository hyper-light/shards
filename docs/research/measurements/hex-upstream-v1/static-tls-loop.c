/* Hex's GPU patch 0008 (R09): what a render-server context does to the host's Vulkan, over and
 * over, as each context does in one GPU VM process: open the loader, make an instance, list its
 * GPUs, destroy it, close the loader. With HOLD set, one instance made first stays alive, as
 * the renderer's HeldDriver keeps it. Prints each round whose enumeration fails, and stops at
 * the first. VK_LOADER_DEBUG=error makes the loader say why (e.g. "cannot allocate memory in
 * static TLS block").
 *   cc -O1 -o static-tls-loop static-tls-loop.c -ldl && ./static-tls-loop ROUNDS [hold]
 * Vulkan's types as vulkan_core.h declares them (1.0): only what these calls need. */
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    int32_t s_type;
    const void *next;
    uint32_t flags;
    const void *application;
    uint32_t layers;
    const char *const *layer_names;
    uint32_t extensions;
    const char *const *extension_names;
} InstanceCreateInfo;

typedef int32_t (*Create)(const InstanceCreateInfo *, const void *, void **);
typedef void (*Destroy)(void *, const void *);
typedef int32_t (*Enumerate)(void *, uint32_t *, void **);

static int round_of(void **held_instance, int hold)
{
    void *loader = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!loader) {
        printf("dlopen: %s\n", dlerror());
        return -1;
    }
    Create create = (Create)dlsym(loader, "vkCreateInstance");
    Destroy destroy = (Destroy)dlsym(loader, "vkDestroyInstance");
    Enumerate enumerate = (Enumerate)dlsym(loader, "vkEnumeratePhysicalDevices");
    InstanceCreateInfo info;
    memset(&info, 0, sizeof info);
    info.s_type = 1; /* VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO */
    void *instance = NULL;
    int32_t made = create(&info, NULL, &instance);
    uint32_t count = 0;
    int32_t listed = made == 0 ? enumerate(instance, &count, NULL) : made;
    if (made == 0 && hold && !*held_instance) {
        *held_instance = instance; /* kept, with its loader, for every later round */
        return (int)count;
    }
    if (made == 0)
        destroy(instance, NULL);
    dlclose(loader);
    return listed == 0 ? (int)count : -(int)(listed < 0 ? -listed : listed) - 1000;
}

int main(int argc, char **argv)
{
    int rounds = argc > 1 ? atoi(argv[1]) : 40;
    int hold = argc > 2 && strcmp(argv[2], "hold") == 0;
    void *held = NULL;
    for (int i = 1; i <= rounds; i++) {
        int gpus = round_of(&held, hold);
        if (gpus <= 0) {
            printf("round %d: no GPU listed (%d)\n", i, gpus);
            return 1;
        }
    }
    printf("%d rounds, every one listed its GPUs%s\n", rounds, hold ? " (driver held)" : "");
    return 0;
}
