/* PM M216 (D144): after a host-visible VkDeviceMemory is freed while another mapping of its pages lives
 * (as a guest's stage-2 mapping of a blob does, PM M193), does a new allocation at its
 * address share those pages? Written through the new one, is the old mapping changed? */
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <vulkan/vulkan.h>

int main(void) {
    VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL, "probe", 1, "probe", 1, VK_API_VERSION_1_2};
    VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0, &app, 0, NULL, 0, NULL};
    VkInstance inst;
    if (vkCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) return 1;
    uint32_t n = 1; VkPhysicalDevice pd; vkEnumeratePhysicalDevices(inst, &n, &pd);
    float prio = 1;
    VkDeviceQueueCreateInfo qci = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, 0, 1, &prio};
    const char *dexts[] = {"VK_KHR_portability_subset"};
    VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, NULL, 0, 1, &qci, 0, NULL, 1, dexts, NULL};
    VkDevice dev; if (vkCreateDevice(pd, &dci, NULL, &dev) != VK_SUCCESS) return 1;
    uint64_t sizes[] = {16384, 65536, 1 << 20, 64 << 20};
    for (size_t si = 0; si < 4; si++) {
        uint64_t sz = sizes[si];
        int same_addr = 0, shared = 0, stale_kept = 0;
        for (int round = 0; round < 32; round++) {
            VkMemoryAllocateInfo mai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, sz, 1};
            VkDeviceMemory a; void *pa;
            vkAllocateMemory(dev, &mai, NULL, &a);
            vkMapMemory(dev, a, 0, VK_WHOLE_SIZE, 0, &pa);
            memset(pa, 0xA5, sz);
            mach_vm_address_t held = 0; vm_prot_t cur, max;
            kern_return_t kr = mach_vm_remap(mach_task_self(), &held, sz, 0, VM_FLAGS_ANYWHERE,
                                             mach_task_self(), (mach_vm_address_t)pa, FALSE, &cur, &max, VM_INHERIT_NONE);
            if (kr != KERN_SUCCESS) { printf("remap %d\n", kr); return 1; }
            vkUnmapMemory(dev, a); vkFreeMemory(dev, a, NULL);
            VkDeviceMemory b; void *pb;
            vkAllocateMemory(dev, &mai, NULL, &b);
            vkMapMemory(dev, b, 0, VK_WHOLE_SIZE, 0, &pb);
            if (pb == pa) same_addr++;
            memset(pb, 0x5A, sz);
            const uint8_t *h = (const uint8_t *)held;
            int saw_b = 0, saw_a = 1;
            for (uint64_t i = 0; i < sz; i += 4096) { if (h[i] == 0x5A) saw_b = 1; if (h[i] != 0xA5) saw_a = 0; }
            shared += saw_b; stale_kept += saw_a;
            vkUnmapMemory(dev, b); vkFreeMemory(dev, b, NULL);
            mach_vm_deallocate(mach_task_self(), held, sz);
        }
        printf("size %llu: of 32, new allocation at the freed one's address %d, its writes seen through the held mapping %d, held mapping kept the old bytes %d\n",
               (unsigned long long)sz, same_addr, shared, stale_kept);
    }
    vkDestroyDevice(dev, NULL); vkDestroyInstance(inst, NULL);
    return 0;
}
