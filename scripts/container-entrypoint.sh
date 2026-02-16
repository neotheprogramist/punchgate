#!/bin/sh
set -e

if [ -n "$PUNCHGATE_GATEWAY" ]; then
    ip route del default 2>/dev/null || true
    ip route add default via "$PUNCHGATE_GATEWAY"

    # Block the container-runtime bridge gateway to prevent cross-bridge
    # routing that bypasses the NAT gateway. In rootless podman, "internal"
    # networks still allow inter-bridge traffic masqueraded from the bridge
    # gateway (.1). Since our NAT gateway uses .2, any traffic from .1 is
    # host-routed cross-bridge traffic that should be dropped.
    BRIDGE_GW=$(echo "$PUNCHGATE_GATEWAY" | sed 's/\.[0-9]*$/.1/')
    iptables -A INPUT -s "$BRIDGE_GW" -j DROP 2>/dev/null || true
fi

exec su-exec punchgate /usr/local/bin/punchgate "$@"
