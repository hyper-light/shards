// Can the host map, at an address of its choosing, the host-visible memory a Vulkan
// driver exports as Venus's renderer exports it (vkr_device_memory.c: a dma-buf where the
// driver exports one, else an opaque fd)? For each device and host-visible memory type:
// export each way, mmap the fd (MAP_FIXED into a reserved range, as a GPU window would),
// write through it, read through vkMapMemory, and read again after vkFreeMemory.
#include <vulkan/vulkan.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

static double now_us(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
}

int main(void) {
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_1};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
    VkInstance inst;
    if (vkCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) { printf("no instance\n"); return 1; }
    uint32_t n = 8;
    VkPhysicalDevice pds[8];
    vkEnumeratePhysicalDevices(inst, &n, pds);
    for (uint32_t d = 0; d < n; d++) {
        VkPhysicalDeviceProperties p;
        vkGetPhysicalDeviceProperties(pds[d], &p);
        printf("device %u: %s (vendor %#x, driver %#x)\n", d, p.deviceName, p.vendorID, p.driverVersion);
        const VkExternalMemoryHandleTypeFlagBits kinds[2] = {VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                                                             VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT};
        const char *names[2] = {"dma-buf", "opaque"};
        for (int k = 0; k < 2; k++) {
            VkPhysicalDeviceExternalBufferInfo bi = {
                .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_BUFFER_INFO,
                .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                .handleType = kinds[k]};
            VkExternalBufferProperties bp = {.sType = VK_STRUCTURE_TYPE_EXTERNAL_BUFFER_PROPERTIES};
            vkGetPhysicalDeviceExternalBufferProperties(pds[d], &bi, &bp);
            printf("  %s buffers: features %#x, export from %#x\n", names[k],
                   bp.externalMemoryProperties.externalMemoryFeatures,
                   bp.externalMemoryProperties.exportFromImportedHandleTypes);
        }
        float prio = 1.0f;
        VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1, .pQueuePriorities = &prio};
        const char *exts[] = {"VK_KHR_external_memory_fd", "VK_EXT_external_memory_dma_buf"};
        VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1,
                                  .pQueueCreateInfos = &qci, .enabledExtensionCount = 2, .ppEnabledExtensionNames = exts};
        VkDevice dev;
        VkResult r = vkCreateDevice(pds[d], &dci, NULL, &dev);
        if (r != VK_SUCCESS) { printf("  no device (%d)\n", r); continue; }
        PFN_vkGetMemoryFdKHR getfd = (PFN_vkGetMemoryFdKHR)vkGetDeviceProcAddr(dev, "vkGetMemoryFdKHR");
        VkPhysicalDeviceMemoryProperties mp;
        vkGetPhysicalDeviceMemoryProperties(pds[d], &mp);
        const VkDeviceSize size = 1 << 20;
        // The window's reserve, as a GPU window would hold its range.
        uint8_t *window = mmap(NULL, 64 << 20, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        for (uint32_t t = 0; t < mp.memoryTypeCount; t++) {
            VkMemoryPropertyFlags f = mp.memoryTypes[t].propertyFlags;
            if (!(f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)) continue;
            for (int k = 0; k < 2; k++) {
                VkExportMemoryAllocateInfo ex = {.sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO, .handleTypes = kinds[k]};
                VkMemoryAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &ex,
                                           .allocationSize = size, .memoryTypeIndex = t};
                VkDeviceMemory mem;
                r = vkAllocateMemory(dev, &ai, NULL, &mem);
                if (r != VK_SUCCESS) { printf("  type %u (flags %#x) %s: allocate %d\n", t, f, names[k], r); continue; }
                VkMemoryGetFdInfoKHR gi = {.sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR, .memory = mem, .handleType = kinds[k]};
                int fd = -1;
                r = getfd(dev, &gi, &fd);
                if (r != VK_SUCCESS) { printf("  type %u (flags %#x) %s: export %d\n", t, f, names[k], r); vkFreeMemory(dev, mem, NULL); continue; }
                off_t len = lseek(fd, 0, SEEK_END);
                uint8_t *at = window + (uint64_t)t * 4 * size + (uint64_t)k * size;
                double t0 = now_us();
                void *m = mmap(at, size, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_FIXED, fd, 0);
                double t1 = now_us();
                int err = errno;
                if (m == MAP_FAILED) {
                    printf("  type %u (flags %#x) %s: fd size %lld, mmap FAILED: %s\n", t, f, names[k], (long long)len, strerror(err));
                    close(fd);
                    vkFreeMemory(dev, mem, NULL);
                    continue;
                }
                ((volatile uint8_t *)m)[100] = 0xa5;
                ((volatile uint8_t *)m)[size - 1] = 0x5a;
                void *vk = NULL;
                vkMapMemory(dev, mem, 0, size, 0, &vk);
                int same = vk && ((uint8_t *)vk)[100] == 0xa5 && ((uint8_t *)vk)[size - 1] == 0x5a;
                if (vk) vkUnmapMemory(dev, mem);
                vkFreeMemory(dev, mem, NULL);
                close(fd);
                // Freed by the driver, still mapped here: the mapping holds the memory.
                int held = ((volatile uint8_t *)m)[100] == 0xa5;
                printf("  type %u (flags %#x) %s: fd size %lld, mmap at window+%#llx in %.1f us, vkMapMemory sees it: %s, after free: %s\n",
                       t, f, names[k], (long long)len, (unsigned long long)(at - window), t1 - t0, same ? "yes" : "NO",
                       held ? "still mapped, same bytes" : "changed");
                mmap(at, size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED | MAP_NORESERVE, -1, 0);
            }
        }
        vkDestroyDevice(dev, NULL);
    }
    vkDestroyInstance(inst, NULL);
    return 0;
}
