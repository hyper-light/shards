/* Exact native properties using the pinned Vulkan headers' complete ABI. No
 * guessed Rust VkPhysicalDeviceProperties tail or driver/model fallback. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan_core.h>

struct shards_gpu_properties {
    uint8_t uuid[VK_UUID_SIZE];
    uint8_t luid[VK_LUID_SIZE];
    uint32_t domain, bus, device, function;
    uint32_t luid_valid, pci_valid, host_memory_valid;
    uint8_t name[VK_MAX_PHYSICAL_DEVICE_NAME_SIZE];
};

_Static_assert(sizeof(struct shards_gpu_properties) == 308,
               "Rust/C native GPU properties must have the same ABI");

int32_t
shards_gpu_get_properties(PFN_vkGetPhysicalDeviceProperties2 properties,
                         PFN_vkEnumerateDeviceExtensionProperties extensions,
                         VkPhysicalDevice device,
                         struct shards_gpu_properties *out)
{
    if (!properties || !extensions || !device || !out)
        return VK_ERROR_INITIALIZATION_FAILED;
    memset(out, 0, sizeof(*out));
    uint32_t count = 0;
    VkResult result = extensions(device, NULL, &count, NULL);
    if (result != VK_SUCCESS)
        return result;
    VkExtensionProperties *exts = NULL;
    if (count) {
        if (sizeof(*exts) > SIZE_MAX / count)
            return VK_ERROR_OUT_OF_HOST_MEMORY;
        exts = calloc(count, sizeof(*exts));
        if (!exts)
            return VK_ERROR_OUT_OF_HOST_MEMORY;
        uint32_t written = count;
        result = extensions(device, NULL, &written, exts);
        if (result != VK_SUCCESS || written > count) {
            free(exts);
            return result == VK_SUCCESS ? VK_ERROR_INITIALIZATION_FAILED : result;
        }
        for (uint32_t i = 0; i < written; ++i) {
            if (memchr(exts[i].extensionName, '\0', VK_MAX_EXTENSION_NAME_SIZE) &&
                strcmp(exts[i].extensionName, VK_EXT_PCI_BUS_INFO_EXTENSION_NAME) == 0)
                out->pci_valid = 1;
            if (memchr(exts[i].extensionName, '\0', VK_MAX_EXTENSION_NAME_SIZE) &&
                strcmp(exts[i].extensionName, VK_EXT_EXTERNAL_MEMORY_HOST_EXTENSION_NAME) == 0)
                out->host_memory_valid = 1;
        }
        free(exts);
    }
    VkPhysicalDevicePCIBusInfoPropertiesEXT pci = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PCI_BUS_INFO_PROPERTIES_EXT,
    };
    VkPhysicalDeviceIDProperties ids = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES,
        .pNext = out->pci_valid ? &pci : NULL,
    };
    VkPhysicalDeviceProperties2 props = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
        .pNext = &ids,
    };
    properties(device, &props);
    if (!memchr(props.properties.deviceName, '\0', VK_MAX_PHYSICAL_DEVICE_NAME_SIZE))
        return VK_ERROR_INITIALIZATION_FAILED;
    memcpy(out->name, props.properties.deviceName, sizeof(out->name));
    memcpy(out->uuid, ids.deviceUUID, sizeof(out->uuid));
    memcpy(out->luid, ids.deviceLUID, sizeof(out->luid));
    out->luid_valid = ids.deviceLUIDValid;
    if (out->pci_valid) {
        out->domain = pci.pciDomain;
        out->bus = pci.pciBus;
        out->device = pci.pciDevice;
        out->function = pci.pciFunction;
    }
    return VK_SUCCESS;
}
