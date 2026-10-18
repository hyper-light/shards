#!/bin/sh
# In the microVM, once (run.sh): for each CPU named, its root and the input in the guest's
# memory, /proc in it, and its qemu-user registered as Docker's binfmt tool registers one
# (flags CFP), each CPU's ELF magic and mask as qemu's scripts/qemu-binfmt-conf.sh (v10.2.3)
# has them, written as they are (the kernel reads their \x escapes).
set -eu
mount -t binfmt_misc none /proc/sys/fs/binfmt_misc 2>/dev/null || true
dd if=/dev/urandom of=/tmp/z bs=1M count=32 2>/dev/null
for cpu in "$@"; do
	cp -a "/x/root-$cpu" "/tmp/root-$cpu"
	mount -t proc proc "/tmp/root-$cpu/proc"
	cp /tmp/z "/tmp/root-$cpu/tmp/z"
	cp "/x/qemu-$cpu" "/tmp/qemu-$cpu"
	case $cpu in
	x86_64)
		magic='\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00'
		mask='\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff'
		;;
	arm)
		magic='\x7fELF\x01\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x28\x00'
		mask='\xff\xff\xff\xff\xff\xff\xff\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff'
		;;
	*)
		echo "setup.sh: no CPU $cpu" >&2
		exit 1
		;;
	esac
	printf '%s' ":qemu-$cpu:M::$magic:$mask:/tmp/qemu-$cpu:CFP" > /proc/sys/fs/binfmt_misc/register
done
