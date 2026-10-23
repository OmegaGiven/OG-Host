#!/bin/bash
# ready.sh NAME SSHPORT UIPORT: is the platform's management up? (web UI, or its CLI for Incus/Debian)
case $1 in
  pve|pve2|og|og2) curl -sk -o /dev/null --max-time 1 https://127.0.0.1:$3/ ;;
  incus) ./pssh $2 'incus list -f csv >/dev/null' 2>/dev/null ;;
  deb) ./pssh $2 'virsh -q list >/dev/null && docker ps -q >/dev/null' 2>/dev/null ;;
esac
