#!/bin/sh
# Install the environment service from this directory (an unpacked release tarball): the binary, the systemd
# units, a system user, settings templates and, if there is none, a new cluster token. Run as root.
#
#   ./install.sh node          # an environment node
#   ./install.sh coordinator   # the coordinator of a cluster
#
# Nothing is started: edit /etc/selfdb/*.env, then `systemctl enable --now selfdb-<role>`.
set -eu
role=${1:?usage: install.sh node|coordinator}
case "$role" in node|coordinator) ;; *) echo "install.sh: node or coordinator" >&2; exit 2 ;; esac
[ "$(id -u)" = 0 ] || { echo "install.sh: run as root" >&2; exit 1; }
here=$(cd "$(dirname "$0")" && pwd)

install -m 0755 "$here/bin/self" /usr/local/bin/self
if ! id selfdb >/dev/null 2>&1; then
    useradd --system --home-dir /var/lib/selfdb --shell /usr/sbin/nologin selfdb
fi
install -d -m 0700 -o selfdb -g selfdb /var/lib/selfdb
install -d -m 0755 /etc/selfdb
install -m 0644 "$here/systemd/selfdb-$role.service" /etc/systemd/system/

# the cluster token: random, readable by root only (systemd hands it to the service as a credential)
if [ ! -e /etc/selfdb/cluster-token ]; then
    (umask 077; head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > /etc/selfdb/cluster-token)
    echo "made /etc/selfdb/cluster-token: copy it to every node and the coordinator (0600, root)"
fi

if [ "$role" = node ] && [ ! -e /etc/selfdb/node.env ]; then
    cat > /etc/selfdb/node.env <<'ENV'
# Arguments of `self env-serve` (see docs/env.md). On its own:
#   SELFDB_NODE_ARGS=--node n1 --listen 0.0.0.0:8650 --max-sessions 400 --metrics-listen 127.0.0.1:9650
# In a cluster, also --coordinator URL --advertise URL (this node's URL as the others reach it).
SELFDB_NODE_ARGS=--node n1 --listen 127.0.0.1:8650 --metrics-listen 127.0.0.1:9650
ENV
fi
if [ "$role" = coordinator ] && [ ! -e /etc/selfdb/coordinator.env ]; then
    cat > /etc/selfdb/coordinator.env <<'ENV'
# Arguments of `self env-coordinator` (see docs/env.md).
SELFDB_COORDINATOR_ARGS=--id c1 --listen 0.0.0.0:8650 --metrics-listen 127.0.0.1:9651
ENV
fi
systemctl daemon-reload

if [ "$role" = node ]; then
    echo "checking this host:"
    su -s /bin/sh selfdb -c "/usr/local/bin/self env-doctor --data /var/lib/selfdb/node" || \
        echo "some checks failed: see above (the cgroups check passes once the service runs with Delegate=yes)"
fi
echo "next: edit /etc/selfdb/$role.env, then: systemctl enable --now selfdb-$role"
