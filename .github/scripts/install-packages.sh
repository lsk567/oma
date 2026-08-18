#!/usr/bin/env bash
# Install packages the runner does not already have.
#
# `apt-get` competes with the unattended-upgrades already running on a fresh
# GitHub runner for the dpkg lock, and by default it waits for that lock with no
# bound. That is how installing tmux came to hold a job for six hours until the
# 6h ceiling killed it, twice in one day, on a package the job needs for about a
# minute of work.
#
# Three guards, in the order they help:
#   - skip entirely when the tool is already on the image, which is the common
#     case and costs nothing;
#   - bound the wait for the lock, so a contended runner fails in two minutes
#     rather than occupying one for six hours;
#   - say up front that nothing may prompt, since a prompt behind `-y` waits
#     for input that is never coming.
#
# Takes command names, not package names. Every package we install is named
# after the binary it provides; one that is not would need looking up here.
set -euo pipefail

missing=()
for command in "$@"; do
  if ! command -v "$command" >/dev/null 2>&1; then
    missing+=("$command")
  fi
done

if [ ${#missing[@]} -eq 0 ]; then
  echo "already present: $*"
  exit 0
fi

echo "installing: ${missing[*]}"
export DEBIAN_FRONTEND=noninteractive
sudo apt-get -o DPkg::Lock::Timeout=120 update
sudo apt-get -o DPkg::Lock::Timeout=120 install -y "${missing[@]}"
