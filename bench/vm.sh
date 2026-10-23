#!/bin/bash
# vm.sh NAME SSHPORT UIPORT [extra qemu args...]  — the benchmark machine: 4 vCPU, 8 GB, 64 GB virtio, UEFI, cores ${CORES:-8-11}
N=$1; SP=$2; UP=$3; shift 3
cd ~/bench-vms
[ -f $N.qcow2 ] || qemu-img create -q -f qcow2 $N.qcow2 64G
[ -f $N.vars ] || cp /usr/share/edk2/x64/OVMF_VARS.4m.fd $N.vars
# the cluster LAN (QEMU multicast socket), always there so cluster members boot as they would
case $N in pve) CL="-netdev socket,id=n1,mcast=230.0.0.1:12341 -device virtio-net-pci,netdev=n1,mac=52:54:00:aa:01:01";;
  pve2) CL="-netdev socket,id=n1,mcast=230.0.0.1:12341 -device virtio-net-pci,netdev=n1,mac=52:54:00:aa:01:02";;
  og) CL="-netdev socket,id=n1,mcast=230.0.0.1:12342 -device virtio-net-pci,netdev=n1,mac=52:54:00:aa:02:01";;
  og2) CL="-netdev socket,id=n1,mcast=230.0.0.1:12342 -device virtio-net-pci,netdev=n1,mac=52:54:00:aa:02:02";;
  # the two plain-Debian hosts keep their cloud-init seed attached (as on first boot)
  deb|incus) CL="-drive file=$N-seed.img,format=raw,if=virtio,readonly=on";;
esac
exec taskset -c ${CORES:-8-11} qemu-system-x86_64 -name $N -enable-kvm -machine q35 -cpu host -smp 4 -m 8192 \
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd -drive if=pflash,format=raw,file=$N.vars \
  -drive file=$N.qcow2,if=virtio,cache=none,aio=io_uring,discard=unmap \
  -netdev user,id=n0,hostfwd=tcp:127.0.0.1:$SP-:22,hostfwd=tcp:127.0.0.1:$UP-:8006 -device virtio-net-pci,netdev=n0 \
  -display none -monitor unix:$N.mon,server,nowait -serial file:$N.serial $CL "$@"
