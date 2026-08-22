#!/bin/sh
# PID 1. No systemd: mount the pseudo-filesystems, bring up eth0, start sshd,
# then sit in a shell so the console is usable.
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2>/dev/null || true
ip link set lo up
ip link set eth0 up 2>/dev/null || true
/usr/sbin/sshd
echo "HEYVM_READY"
exec /bin/sh
