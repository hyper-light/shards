#!/bin/sh
# In the microVM: host.py's runs of CPU $1's programs, one a line, as M241's serve.sh: host.py
# names a workload, this answers B, host.py samples the VM process and answers, this runs it
# and answers E with its wall, user and sys seconds, or E FAIL and why.
cpu=$1
loop='i=0; while [ $i -lt 100 ]; do /bin/true; i=$((i+1)); done'
while read -r what; do
	echo B
	read -r _
	case $what in
	native-sha256) set -- sha256sum /tmp/z ;;
	emulated-sha256) set -- chroot "/tmp/root-$cpu" /bin/busybox sha256sum /tmp/z ;;
	native-gzip) set -- gzip -c /tmp/z ;;
	emulated-gzip) set -- chroot "/tmp/root-$cpu" /bin/busybox gzip -c /tmp/z ;;
	native-starts) set -- sh -c "$loop" ;;
	emulated-starts) set -- chroot "/tmp/root-$cpu" /bin/sh -c "$loop" ;;
	*)
		echo "E FAIL no workload $what"
		continue
		;;
	esac
	if time -p -o /tmp/t "$@" >/dev/null 2>/tmp/err; then
		echo "E $(awk '/^real/ {r=$2} /^user/ {u=$2} /^sys/ {s=$2} END {print r, u, s}' /tmp/t)"
	else
		echo "E FAIL $(tr '\n' ' ' </tmp/err)"
	fi
done
