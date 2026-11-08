#!/bin/sh
# Windows 11 arm64 under QEMU on Hypervisor.framework, the step 10 host for shards' arm64
# WHP backend. NESTED=1 gives the guest EL2 (virtualization=on: macOS 15+ on M3 or
# later), which Windows' hypervisor needs; INSTALL=1 attaches the Windows ISO ($ISO) and
# the setup media. Display on VNC 127.0.0.1:5907, monitor on ./monitor.sock, ssh on
# 127.0.0.1:2222.
set -eu
cd "$(dirname "$0")"
machine=virt,gic-version=3,highmem=on
if [ "${SECURE:-0}" = 1 ]; then
    machine="$machine,secure=on"
fi
if [ "${NESTED:-0}" = 1 ]; then
    machine="$machine,virtualization=on"
fi
set -- \
    -L /opt/homebrew/share/qemu -M "$machine" -accel "${ACCEL:-hvf}" -cpu "${CPU:-host}" -smp "${CPUS:-8}" -m "${MEM:-16G}" \
    -drive "if=pflash,format=raw,readonly=on,file=${FW_CODE:-code.fd}" \
    -drive "if=pflash,format=raw,file=${VARS:-vars.fd}" \
    -device ramfb -device qemu-xhci -device usb-kbd -device usb-tablet \
    -drive "if=none,id=hd,file=${VM_DISK:-win.qcow2},format=qcow2,discard=unmap" \
    -device nvme,drive=hd,serial=shards0,bootindex=1 \
    -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2222-:22 -device virtio-net-pci,netdev=n0 \
    -boot menu=on,splash-time=0 \
    -rtc base=localtime,clock=host \
    -vnc 127.0.0.1:7 -monitor unix:monitor.sock,server,nowait \
    -chardev socket,id=serial0,path=serial.sock,server=on,wait=off,logfile=serial.log \
    -serial chardev:serial0
if [ "${INSTALL:-0}" = 1 ]; then
    set -- "$@" \
        -drive if=none,id=cd,media=cdrom,readonly=on,file="$ISO" \
        -device usb-storage,drive=cd,bootindex=0 \
        -drive if=none,id=setup,media=cdrom,readonly=on,file=media.iso \
        -device usb-storage,drive=setup
fi
exec "${QEMU_BIN:-qemu-system-aarch64}" "$@"
