// E6: host-visible memory from the caller's view. Each of N allocations of SIZE bytes is
// timed through vkAllocateMemory, its first vkMapMemory, the first write to its first and
// last bytes (the pages' first faults), vkUnmapMemory and vkFreeMemory; printed per step
// as n, p50, p90, p99 and max in microseconds. Through Venus (D144), allocating makes a
// blob, mapping maps it into the guest's window (hv_vm_map) and freeing unmaps it.
//   blobs [N]
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <vulkan/vulkan.h>

static double now_us(void) {
#ifdef __APPLE__
    // macOS's CLOCK_MONOTONIC counts whole microseconds; this, nanoseconds.
    return (double)clock_gettime_nsec_np(CLOCK_UPTIME_RAW) / 1e3;
#else
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double)t.tv_sec * 1e6 + (double)t.tv_nsec / 1e3;
#endif
}

static int cmp(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}

static void report(const char *step, unsigned long size, double *v, int n) {
    qsort(v, (size_t)n, sizeof *v, cmp);
    printf("%-8s %9lu n=%d p50=%.1f p90=%.1f p99=%.1f max=%.1f\n", step, size, n, v[n / 2],
           v[n * 90 / 100], v[n * 99 / 100], v[n - 1]);
}

#define CHECK(x)                                                                             \
    do {                                                                                     \
        VkResult r_ = (x);                                                                   \
        if (r_ != VK_SUCCESS) {                                                              \
            fprintf(stderr, "%s: %d\n", #x, r_);                                             \
            return 1;                                                                        \
        }                                                                                    \
    } while (0)

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 200;
    if (n < 1) return 2;
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_1};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
    // A loader enumerates a portability driver (MoltenVK's) only where the instance asks
    // for them; MoltenVK called directly has no such extension, and needs none.
    const char *ext[] = {VK_KHR_PORTABILITY_ENUMERATION_EXTENSION_NAME};
    uint32_t n_ext = 0;
    vkEnumerateInstanceExtensionProperties(NULL, &n_ext, NULL);
    VkExtensionProperties *have = calloc(n_ext ? n_ext : 1, sizeof *have);
    if (!have) return 1;
    vkEnumerateInstanceExtensionProperties(NULL, &n_ext, have);
    for (uint32_t i = 0; i < n_ext; i++)
        if (!strcmp(have[i].extensionName, ext[0])) {
            ici.flags = VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR;
            ici.enabledExtensionCount = 1;
            ici.ppEnabledExtensionNames = ext;
        }
    free(have);
    VkInstance inst;
    CHECK(vkCreateInstance(&ici, NULL, &inst));
    uint32_t count = 1;
    VkPhysicalDevice pd;
    VkResult r = vkEnumeratePhysicalDevices(inst, &count, &pd);
    if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || count == 0) {
        fprintf(stderr, "no Vulkan device\n");
        return 1;
    }
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(pd, &props);
    printf("device %s\n", props.deviceName);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    VkMemoryPropertyFlags want = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    uint32_t type = UINT32_MAX;
    for (uint32_t i = 0; i < mp.memoryTypeCount && type == UINT32_MAX; i++)
        if ((mp.memoryTypes[i].propertyFlags & want) == want) type = i;
    if (type == UINT32_MAX) {
        fprintf(stderr, "no host-visible, coherent memory\n");
        return 1;
    }
    float prio = 1.0f;
    VkDeviceQueueCreateInfo q = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1,
                                 .pQueuePriorities = &prio};
    VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1,
                              .pQueueCreateInfos = &q};
    VkDevice dev;
    CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
    static const unsigned long sizes[] = {4096, 65536, 1UL << 20, 16UL << 20, 64UL << 20};
    static const char *steps[] = {"allocate", "map", "touch", "unmap", "free"};
    double *t[5];
    for (int k = 0; k < 5; k++)
        if (!(t[k] = malloc(sizeof(double) * (size_t)n))) return 1;
    for (size_t s = 0; s < sizeof sizes / sizeof *sizes; s++) {
        for (int i = 0; i < n; i++) {
            VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                        .allocationSize = sizes[s], .memoryTypeIndex = type};
            VkDeviceMemory m;
            void *p;
            double t0 = now_us();
            CHECK(vkAllocateMemory(dev, &mai, NULL, &m));
            double t1 = now_us();
            CHECK(vkMapMemory(dev, m, 0, VK_WHOLE_SIZE, 0, &p));
            double t2 = now_us();
            ((volatile char *)p)[0] = 1;
            ((volatile char *)p)[sizes[s] - 1] = 1;
            double t3 = now_us();
            vkUnmapMemory(dev, m);
            double t4 = now_us();
            vkFreeMemory(dev, m, NULL);
            double t5 = now_us();
            t[0][i] = t1 - t0, t[1][i] = t2 - t1, t[2][i] = t3 - t2, t[3][i] = t4 - t3, t[4][i] = t5 - t4;
        }
        for (int k = 0; k < 5; k++) report(steps[k], sizes[s], t[k], n);
    }
    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(inst, NULL);
    return 0;
}
