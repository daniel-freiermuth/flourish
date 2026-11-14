#!/usr/bin/env bash
# Run the command in a cgroup hunter's tests may manage: every process the
# daemon starts, a test's own children included, runs in a cgroup of its
# own below the one it runs in (src/child.rs).
#
# Where the current cgroup is already writable -- the same check the
# daemon makes at startup (`child::start`) -- the command runs in place,
# so that it stays inside whatever contains it: a hunter worker running
# this repo's gate stays in its worker cgroup, which a cap, the wall clock
# or the daemon's stop ends. Only elsewhere (CI's runner, a login shell in
# a cgroup systemd does not hand over) does it go into a new delegated
# scope of the user's systemd.
set -euo pipefail

cgroup="/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)"
if [ -w "$cgroup" ] && [ -w "$cgroup/cgroup.procs" ]; then
  exec "$@"
fi
exec systemd-run --user --scope --quiet -p Delegate=yes "$@"
