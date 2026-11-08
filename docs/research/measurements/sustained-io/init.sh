#!/bin/sh
# PID 1 of the benchmark's microVMs that boot the image as an ext4 root on virtio-blk
# (Firecracker, Cloud Hypervisor, and shards' VMM given the same disks: run.py). It mounts
# what a container's runtime mounts, mounts the volume (sio.vol: disk, ext4 on /dev/vdb;
# fs, virtio-fs tagged vol), runs work.sh in a cgroup of its own with the image's
# environment (/sio/env), and ends the machine: ACPI power-off, or a reset where the VMM
# ends on that (sio.off=reboot: Firecracker).
PATH=/usr/sbin:/usr/bin:/sbin:/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts /dev/shm
mount -t devpts devpts /dev/pts
mount -t tmpfs tmpfs /dev/shm
mount -t tmpfs tmpfs /tmp
mount -t tmpfs tmpfs /run
mount -t cgroup2 cgroup2 /sys/fs/cgroup
workload= place= vol= off=
for kv in $(cat /proc/cmdline); do
    case $kv in
    sio.workload=*) workload=${kv#*=} ;;
    sio.place=*) place=${kv#*=} ;;
    sio.vol=*) vol=${kv#*=} ;;
    sio.off=*) off=${kv#*=} ;;
    esac
done
mkdir -p /vol
case $vol in
disk) mount -t ext4 /dev/vdb /vol ;;
fs) mount -t virtiofs vol /vol ;;
esac
echo +memory +cpu >/sys/fs/cgroup/cgroup.subtree_control
mkdir /sys/fs/cgroup/work
# root's HOME, as Docker gives a process of an image that names none.
export HOME=/root
sh -c 'echo $$ >/sys/fs/cgroup/work/cgroup.procs && . /sio/env && cd / && exec /sio/work "$@"' \
    work "$workload" "$place" >/dev/console 2>&1
echo "SIO-STATUS $?" >/dev/console
case $vol in disk | fs) umount /vol ;; esac
sync
if [ "$off" = reboot ]; then echo b >/proc/sysrq-trigger; else echo o >/proc/sysrq-trigger; fi
# A power-off by sysrq is work the kernel does later; PID 1 ending first is a panic.
while :; do sleep 60; done
