#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Runs inside the outer container `run.sh` starts: a Docker daemon of its own,
# then one `cargo test` per argument, each argument being that run's cargo
# test arguments.
set -u

# Apple's `container` VMs: the kernel has no nf_tables, which dockerd's NAT
# needs, and /proc/sys comes up read-only, so dockerd cannot enable IP
# forwarding. Docker Desktop needs neither change.
if ! iptables -t nat -L -n >/dev/null 2>&1; then
  update-alternatives --set iptables /usr/sbin/iptables-legacy >/dev/null
  update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy >/dev/null
fi
[ -w /proc/sys/net/ipv4/ip_forward ] || mount -o remount,rw /proc/sys

dockerd >/logs/dockerd.log 2>&1 &
for _ in $(seq 1 60); do docker info >/dev/null 2>&1 && break; sleep 1; done
docker info >/dev/null 2>&1 || { tail -30 /logs/dockerd.log; exit 1; }

# A writable copy of the source: some tests make their temp dirs in the
# current directory, and the source is mounted read-only.
mkdir -p /work
tar -C /src --exclude=./target --exclude=./.claude -cf - . | tar -C /work -xf -
cd /work

rc=0
n=0
for args in "$@"; do
  n=$((n + 1))
  log=/logs/test-$n.log
  echo "=== cargo test --locked $args"
  eval cargo test --locked "$args" >"$log" 2>&1
  r=$?
  grep -E "^test .*FAILED$|^test result|panicked at" "$log" | grep -v " 0 passed; 0 failed"
  if [ $r != 0 ]; then
    rc=1
    echo "--- exit $r; full log: target/linux-tests/test-$n.log"
  fi
done
exit $rc
