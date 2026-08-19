#!/usr/bin/env bash
# Stop apt waiting forever for the dpkg lock, whoever runs it.
#
# A fresh GitHub runner is already running unattended-upgrades, and apt waits
# for the lock unbounded by default -- so losing that race is not an error, it
# is a hang, and the job's ceiling is the only thing that ends it.
#
# Configured on the runner rather than passed at the call site, because the apt
# that hangs most is not ours: `playwright install --with-deps` shells out to
# apt several layers down and takes no flags from us. A file here reaches that
# one too.
set -euo pipefail

sudo tee /etc/apt/apt.conf.d/99-omar-ci >/dev/null <<'CONF'
DPkg::Lock::Timeout "120";
Acquire::Retries "3";
CONF

echo "apt: lock wait bounded at 120s, 3 retries on fetch"
