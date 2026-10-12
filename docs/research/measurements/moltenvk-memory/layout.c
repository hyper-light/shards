/* PM M216 (D144): MoltenVK's host-visible VkDeviceMemory, mapped as
 * virglrenderer's Venus maps a blob's (vkMapMemory of all of it). Is each mapping 16 KiB
 * aligned, its own VM region, and are a mapping's pages past its size its own (no other
 * allocation in them)? Are freed allocations' addresses reused? */
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define PAGE 16384ull
#define N 64

static int region(uint64_t addr, uint64_t *start, uint64_t *size, uint32_t *tag) {
    mach_vm_address_t a = addr;
    mach_vm_size_t s = 0;
    vm_region_extended_info_data_t info;
    mach_msg_type_number_t count = VM_REGION_EXTENDED_INFO_COUNT;
    mach_port_t obj = MACH_PORT_NULL;
    kern_return_t kr = mach_vm_region(mach_task_self(), &a, &s, VM_REGION_EXTENDED_INFO,
                                      (vm_region_info_t)&info, &count, &obj);
    if (kr != KERN_SUCCESS) return -1;
    *start = a; *size = s; *tag = info.user_tag;
    return 0;
}

int main(void) {
    VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL, "probe", 1, "probe", 1, VK_API_VERSION_1_2};
    const char *exts[] = {NULL};
    VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL,
                                0, &app, 0, NULL, 0, NULL};
    VkInstance inst;
    if (vkCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) { printf("instance\n"); return 1; }
    uint32_t n = 1; VkPhysicalDevice pd;
    vkEnumeratePhysicalDevices(inst, &n, &pd);
    VkPhysicalDeviceProperties props; vkGetPhysicalDeviceProperties(pd, &props);
    printf("device %s, minMemoryMapAlignment %zu, nonCoherentAtomSize %llu\n", props.deviceName,
           props.limits.minMemoryMapAlignment, (unsigned long long)props.limits.nonCoherentAtomSize);
    VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    for (uint32_t i = 0; i < mp.memoryHeapCount; i++)
        printf("heap %u: %llu bytes, flags %x\n", i, (unsigned long long)mp.memoryHeaps[i].size, mp.memoryHeaps[i].flags);
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        printf("type %u: heap %u flags %x\n", i, mp.memoryTypes[i].heapIndex, mp.memoryTypes[i].propertyFlags);
    float prio = 1;
    VkDeviceQueueCreateInfo qci = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, 0, 1, &prio};
    const char *dexts[] = {"VK_KHR_portability_subset"};
    VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, NULL, 0, 1, &qci, 0, NULL, 1, dexts, NULL};
    VkDevice dev;
    if (vkCreateDevice(pd, &dci, NULL, &dev) != VK_SUCCESS) { printf("device\n"); return 1; }
    uint64_t sizes[] = {1, 100, 4096, 4097, 16384, 16385, 65536, 65537, 1 << 20};
    for (uint32_t t = 0; t < mp.memoryTypeCount; t++) {
        VkMemoryPropertyFlags f = mp.memoryTypes[t].propertyFlags;
        if (!(f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)) continue;
        for (size_t si = 0; si < sizeof sizes / sizeof sizes[0]; si++) {
            uint64_t sz = sizes[si];
            VkDeviceMemory mem[N]; void *ptr[N];
            int unaligned = 0, shared_page = 0, shared_region = 0, tail_other = 0;
            for (int k = 0; k < N; k++) {
                VkMemoryAllocateInfo mai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, sz, t};
                if (vkAllocateMemory(dev, &mai, NULL, &mem[k]) != VK_SUCCESS) { printf("alloc %llu\n", (unsigned long long)sz); return 1; }
                if (vkMapMemory(dev, mem[k], 0, VK_WHOLE_SIZE, 0, &ptr[k]) != VK_SUCCESS) { printf("map\n"); return 1; }
                if ((uint64_t)ptr[k] % PAGE) unaligned++;
            }
            uint64_t rs0 = 0, rsz0 = 0; uint32_t tag0 = 0;
            for (int k = 0; k < N; k++) {
                uint64_t a0 = (uint64_t)ptr[k] & ~(PAGE - 1), a1 = ((uint64_t)ptr[k] + sz + PAGE - 1) & ~(PAGE - 1);
                for (int j = 0; j < N; j++) {
                    if (j == k) continue;
                    uint64_t b = (uint64_t)ptr[j];
                    if (b + sz > a0 && b < a1) shared_page++;
                }
                uint64_t rs, rsz; uint32_t tag;
                if (region((uint64_t)ptr[k], &rs, &rsz, &tag) == 0) {
                    if (k == 0) { rs0 = rs; rsz0 = rsz; tag0 = tag; }
                    for (int j = 0; j < N; j++) {
                        if (j == k) continue;
                        uint64_t b = (uint64_t)ptr[j];
                        if (b >= rs && b < rs + rsz) { shared_region++; break; }
                    }
                    if (rs + rsz < a1) tail_other++;
                }
            }
            printf("type %u flags %x size %llu: unaligned %d, pages shared with another %d, regions shared %d, rounded tail past its region %d; first region %llx+%llx tag %u off %llu\n",
                   t, f, (unsigned long long)sz, unaligned, shared_page, shared_region, tail_other,
                   (unsigned long long)rs0, (unsigned long long)rsz0, tag0,
                   (unsigned long long)((uint64_t)ptr[0] - rs0));
            void *old[N]; memcpy(old, ptr, sizeof old);
            for (int k = 0; k < N; k++) { vkUnmapMemory(dev, mem[k]); vkFreeMemory(dev, mem[k], NULL); }
            int reused = 0;
            for (int k = 0; k < N; k++) {
                VkMemoryAllocateInfo mai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, sz, t};
                vkAllocateMemory(dev, &mai, NULL, &mem[k]);
                vkMapMemory(dev, mem[k], 0, VK_WHOLE_SIZE, 0, &ptr[k]);
                for (int j = 0; j < N; j++) if (ptr[k] == old[j]) { reused++; break; }
            }
            printf("    after freeing all, %d of %d new mappings at a freed one's address\n", reused, N);
            for (int k = 0; k < N; k++) { vkUnmapMemory(dev, mem[k]); vkFreeMemory(dev, mem[k], NULL); }
        }
    }
    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(inst, NULL);
    return 0;
}
