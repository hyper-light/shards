// Prints a bookmark for a path, hex-encoded, as an unsandboxed parent passes one to a
// sandboxed child ("Share file access between processes with URL bookmarks": options 0).
#include <CoreFoundation/CoreFoundation.h>
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    CFURLRef u = CFURLCreateFromFileSystemRepresentation(NULL, (const UInt8 *)argv[1], (CFIndex)strlen(argv[1]), false);
    CFErrorRef err = NULL;
    CFDataRef d = CFURLCreateBookmarkData(NULL, u, 0, NULL, NULL, &err);
    if (!d) { fprintf(stderr, "no bookmark\n"); return 1; }
    const UInt8 *b = CFDataGetBytePtr(d);
    for (CFIndex i = 0; i < CFDataGetLength(d); i++) printf("%02x", b[i]);
    printf("\n");
    return 0;
}
