#!/bin/sh
# In the microVM or the container, once (run.sh): the root of CPU $1 and the input in the
# guest's memory, and /proc in the root, which Rosetta reads. With `register`, /x/qemu
# registered for $1's programs as Docker's binfmt tool registers them, flags CFP: credentials
# from the binary, the interpreter opened as it is registered, argv[0] preserved, which
# Docker's builds of qemu expect.
set -eu
cp -a /x/root /tmp/root
mount -t proc proc /tmp/root/proc
dd if=/dev/urandom of=/tmp/z bs=1M count=32 2>/dev/null
cp /tmp/z /tmp/root/tmp/z
[ "${2:-}" = register ] || exit 0
mount -t binfmt_misc none /proc/sys/fs/binfmt_misc 2>/dev/null || true
cp /x/qemu /tmp/qemu
# qemu's scripts/qemu-binfmt-conf.sh: each CPU's ELF magic and mask, which the kernel reads
# with their \x escapes, so the line is written as it is (printf's format would read them).
case $1 in
x86_64)
	magic='\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00'
	mask='\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff'
	;;
aarch64)
	magic='\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\xb7\x00'
	mask='\xff\xff\xff\xff\xff\xff\xff\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff'
	;;
esac
printf '%s' ":qemu-$1:M::$magic:$mask:/tmp/qemu:CFP" > /proc/sys/fs/binfmt_misc/register
