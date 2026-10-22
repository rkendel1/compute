#!/bin/sh
# Build the root filesystem a Compute microVM session boots: BusyBox for the
# shell and tools, and the guest agent as /sbin/compute-guest (PID 1).
#
#   build-rootfs.sh COMPUTE_GUEST BUSYBOX OUTPUT [SIZE_MIB]
#
# COMPUTE_GUEST is a static build of crates/compute-guest
# (cargo build --release -p compute-guest --target x86_64-unknown-linux-musl);
# BUSYBOX is a static busybox. Needs mkfs.ext4 (e2fsprogs >= 1.43 for -d);
# no root, no mounts.
set -eu
guest=$1
busybox=$2
output=$3
size=${4:-512}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for directory in bin sbin usr/bin usr/sbin etc root proc sys dev tmp run workspace; do
  mkdir -p "$work/$directory"
done
cp "$busybox" "$work/bin/busybox"
chmod 0755 "$work/bin/busybox"
for applet in $("$work/bin/busybox" --list); do
  [ -e "$work/bin/$applet" ] || ln -s busybox "$work/bin/$applet"
done
cp "$guest" "$work/sbin/compute-guest"
chmod 0755 "$work/sbin/compute-guest"
printf 'root:x:0:0:root:/root:/bin/sh\n' >"$work/etc/passwd"
printf 'root:x:0:\n' >"$work/etc/group"
printf 'compute\n' >"$work/etc/hostname"
chmod 1777 "$work/tmp"

rm -f "$output"
truncate -s "${size}M" "$output"
mkfs.ext4 -q -F -L compute-rootfs -d "$work" "$output"
echo "$output: $(du -h --apparent-size "$output" | cut -f1) ext4, compute-guest as init"
