// Read-only native Metal device enumeration. No queue, command buffer or GPU allocation.
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

int main(void)
{
    @autoreleasepool {
        NSArray<id<MTLDevice>> *devices = MTLCopyAllDevices();
        NSMutableArray *found = [NSMutableArray array];
        for (id<MTLDevice> device in devices) {
            [found addObject:@{
                @"name": device.name,
                @"registry_id": @(device.registryID),
                @"unified_memory": @(device.hasUnifiedMemory),
                @"low_power": @(device.lowPower),
                @"removable": @(device.removable)
            }];
        }
        NSError *error = nil;
        NSData *data = [NSJSONSerialization dataWithJSONObject:found options:0 error:&error];
        if (!data) {
            fprintf(stderr, "Metal enumeration JSON error\n");
            return 1;
        }
        if (fwrite(data.bytes, 1, data.length, stdout) != data.length || putchar('\n') == EOF)
            return 1;
        return 0;
    }
}
