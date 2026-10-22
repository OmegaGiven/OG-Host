#!/bin/bash
# vm.sh NAME SSHPORT UIPORT [extra qemu args...]  — the benchmark machine: 4 vCPU, 8 GB, 64 GB virtio, UEFI, cores 8-11
N=$1; SP=$2; UP=$3; shift 3
cd ~/bench-vms
[ -f $N.qcow2 ] || qemu-img create -q -f qcow2 $N.qcow2 64G
[ -f $N.vars ] || cp /usr/share/edk2/x64/OVMF_VARS.4m.fd $N.vars
exec taskset -c 8-11 qemu-system-x86_64 -name $N -enable-kvm -machine q35 -cpu host -smp 4 -m 8192 \
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd -drive if=pflash,format=raw,file=$N.vars \
  -drive file=$N.qcow2,if=virtio,cache=none,aio=io_uring,discard=unmap \
  -netdev user,id=n0,hostfwd=tcp:127.0.0.1:$SP-:22,hostfwd=tcp:127.0.0.1:$UP-:8006 -device virtio-net-pci,netdev=n0 \
  -display none -monitor unix:$N.mon,server,nowait -serial file:$N.serial "$@"
