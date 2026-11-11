#!/usr/bin/env bash
# Installs packages on a GitHub-hosted Ubuntu runner. The images' mirror list points at
# azure.archive.ubuntu.com, whose name the runners sometimes cannot resolve (2026-11-11: every
# install failed for hours): the public archive is used instead, and each step is retried.
set -u
opts=(-o Acquire::Retries=3 -o Acquire::http::Timeout=20 -o Acquire::ForceIPv4=true)
sudo sed -i 's#azure\.archive\.ubuntu\.com#archive.ubuntu.com#g' /etc/apt/apt-mirrors.txt /etc/apt/sources.list /etc/apt/sources.list.d/* 2>/dev/null || true
for attempt in 1 2 3; do
  sudo apt-get "${opts[@]}" update || true
  sudo apt-get "${opts[@]}" install -y --no-install-recommends "$@" && exit 0
  # What the resolver says, for the log of a failed run.
  getent hosts archive.ubuntu.com github.com || true
  grep -v "^#" /etc/resolv.conf || true
  sleep $((attempt * 10))
done
exit 1
