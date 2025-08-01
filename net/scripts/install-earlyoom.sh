#!/usr/bin/env bash
set -ex
#
# Install EarlyOOM
#

[[ $(uname) = Linux ]] || exit 1
[[ $USER = root ]] || exit 1

# earlyoom specifically needs "SysRq 64 - enable signalling of processes (term, kill, oom-kill)"
# but for simplicity just enable all SysRq
sysctl -w kernel.sysrq=1
echo kernel.sysrq=1 >> /etc/sysctl.conf

if command -v earlyoom; then
  systemctl status earlyoom
else
  apt-get install --quiet --yes earlyoom

  cat > earlyoom <<OOM
  # trigger at 20% available RAM,
  EARLYOOM_ARGS="-m 20"
OOM
  cp earlyoom /etc/default/
  rm earlyoom

  systemctl stop earlyoom
  systemctl enable earlyoom
  systemctl start earlyoom
fi

