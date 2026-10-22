#!/bin/sh
# isup.sh NAME: is the benchmark VM NAME running
for p in $(pgrep -x qemu-system-x86); do tr '\0' ' ' < /proc/$p/cmdline | grep -q -- "-name $1 " && exit 0; done; exit 1
