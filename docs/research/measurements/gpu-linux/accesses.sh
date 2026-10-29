#!/bin/sh
# What a Linux host's Vulkan loader and GPU drivers open as they start and as Venus's
# renderer would use them (PM M382, D201), under the environment a VM process is given,
# none of the user's: each library the dynamic linker loads (LD_DEBUG=files), each
# manifest the loader reads (VK_LOADER_DEBUG), and every other file, directory and
# device the drivers open, stat or list (openlog.c, preloaded). vulkaninfo, then
# dmabuf-map (M381), which makes a device, allocates and exports its memory.
#   accesses.sh VULKAN_HEADERS_DIR OUT_DIR
set -eu
here=$(cd "$(dirname "$0")" && pwd)
headers=$1 out=$2
mkdir -p "$out"
cc -O1 -Wall -shared -fPIC "$here/openlog.c" -o "$out/openlog.so" -ldl
cc -O1 -Wall -I"$headers/include" "$here/dmabuf-map.c" -o "$out/dmabuf-map" -lvulkan
rm -f "$out/opens.log"
for program in vulkaninfo "$out/dmabuf-map"; do
	arg=
	[ "$program" = vulkaninfo ] && arg=--summary
	env -i PATH=/usr/bin OPENLOG="$out/opens.log" LD_PRELOAD="$out/openlog.so" "$program" $arg >/dev/null 2>&1
done
env -i PATH=/usr/bin VK_LOADER_DEBUG=all LD_DEBUG=files vulkaninfo --summary >/dev/null 2>"$out/loader.log"
echo "# libraries the dynamic linker loaded"
grep -oE 'file=/[^ ;]+' "$out/loader.log" | sort -u | sed 's/^file=//' | xargs -n1 dirname | sort | uniq -c
echo "# manifests the loader found"
grep -oE '/[^ ]+\.json' "$out/loader.log" | sort -u
echo "# everything else opened, stat'd or listed: call, path (PIDs, PCI paths folded), how often, failed or not"
awk '{print $1, $2, ($4 < 0 ? "failed" : "ok")}' "$out/opens.log" |
	grep -v ' /usr/lib/lib' |
	sed -E 's#/proc/[0-9]+/#/proc/PID/#; s#/sys/devices/pci[^ ]*#/sys/devices/pci...#; s#/sys/devices/system/cpu/cpu[0-9]+/#/sys/devices/system/cpu/cpuN/#' |
	sort | uniq -c | sort -rn
