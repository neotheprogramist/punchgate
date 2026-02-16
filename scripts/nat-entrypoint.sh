#!/bin/sh
set -e

# ip_forward is set via compose sysctls directive
apk add --no-cache iptables >/dev/null 2>&1

# Identify interfaces by subnet prefix from env vars
PUBLIC_IF=$(ip -4 -o addr show | grep "${NAT_PUBLIC_SUBNET}" | awk '{print $2}')
LAN_IF=$(ip -4 -o addr show | grep "${NAT_LAN_SUBNET}" | awk '{print $2}')
PUBLIC_IP=$(ip -4 -o addr show dev "$PUBLIC_IF" | awk '{print $4}' | cut -d/ -f1)

echo "NAT: PUBLIC_IF=$PUBLIC_IF LAN_IF=$LAN_IF PUBLIC_IP=$PUBLIC_IP"

# ── Outbound NAT ────────────────────────────────────────────────────────────
# SNAT with fixed port for QUIC (sport 4001) — prevents MASQUERADE from
# picking random ports when conntrack entries race during hole-punching.
iptables -t nat -A POSTROUTING -s "${NAT_LAN_SUBNET}.0/24" -p udp --sport 4001 -o "$PUBLIC_IF" -j SNAT --to-source "${PUBLIC_IP}:4001"
# General MASQUERADE for all other outbound traffic (DNS, etc.)
iptables -t nat -A POSTROUTING -s "${NAT_LAN_SUBNET}.0/24" -o "$PUBLIC_IF" -j MASQUERADE

# ── Inbound NAT (full-cone) ─────────────────────────────────────────────────
# Forward inbound UDP port 4001 to the LAN peer. This simulates full-cone
# (Endpoint-Independent Mapping + Endpoint-Independent Filtering) NAT: once
# a mapping exists, ANY external host can send to the mapped port. The DNAT
# ensures hole-punch packets are forwarded regardless of conntrack timing.
if [ -n "$NAT_PEER_IP" ]; then
    iptables -t nat -A PREROUTING -i "$PUBLIC_IF" -p udp --dport 4001 -j DNAT --to-destination "${NAT_PEER_IP}:4001"
    echo "NAT: DNAT udp/4001 -> ${NAT_PEER_IP}:4001 (full-cone)"
fi

# ── Forwarding rules ────────────────────────────────────────────────────────
# Allow all outbound (LAN -> public)
iptables -A FORWARD -i "$LAN_IF" -o "$PUBLIC_IF" -j ACCEPT
# Allow established/related return traffic (public -> LAN)
iptables -A FORWARD -i "$PUBLIC_IF" -o "$LAN_IF" -m state --state ESTABLISHED,RELATED -j ACCEPT
# Allow DNAT'd inbound on QUIC port (full-cone: new inbound to mapped port)
if [ -n "$NAT_PEER_IP" ]; then
    iptables -A FORWARD -i "$PUBLIC_IF" -o "$LAN_IF" -p udp --dport 4001 -j ACCEPT
fi
# Block all other new incoming from public to LAN
iptables -A FORWARD -i "$PUBLIC_IF" -o "$LAN_IF" -j DROP

echo "NAT gateway ready"
exec sleep infinity
