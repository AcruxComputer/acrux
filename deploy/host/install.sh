#!/usr/bin/env bash
# Prepares a fresh Ubuntu 24.04 host for Acrux: Docker Engine with compose, a firewall that admits
# only SSH, HTTP and HTTPS, and /etc/acrux for the host's secrets. Run as root, once.
set -euo pipefail

apt-get update
apt-get install -y ca-certificates curl ufw
install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" > /etc/apt/sources.list.d/docker.list
apt-get update
apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
systemctl enable --now docker

# Only Caddy's ports and SSH face the internet; the node's QUIC, metrics and the API stay inside Docker.
ufw default deny incoming
ufw default allow outgoing
ufw allow OpenSSH
ufw allow 80/tcp
ufw allow 443/tcp
ufw --force enable

install -m 0700 -d /etc/acrux
echo "Docker $(docker --version), firewall on. Put genesis.json and acrux.env in /etc/acrux (deploy/README.md)."
