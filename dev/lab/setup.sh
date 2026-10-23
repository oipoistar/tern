#!/bin/bash
# Makes the lab's keys (once) and starts the lab. Needs Docker and
# ssh-keygen; run it from anywhere.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p keys

key() {
  local name=$1
  shift
  [ -f "keys/$name" ] || ssh-keygen -q "$@" -f "keys/$name" -C "tern-lab $name"
}
key id_ed25519 -t ed25519 -N ''
key id_ecdsa -t ecdsa -b 256 -N ''
key id_rsa -t rsa -b 3072 -N ''
# Encrypted, with the passphrase `lab-pass`.
key id_locked -t ed25519 -N 'lab-pass'
# Signed by the user CA, and authorized nowhere.
key id_carol -t ed25519 -N ''
key user_ca -t ed25519 -N ''
key host_ca -t ed25519 -N ''
[ -f keys/id_carol-cert.pub ] ||
  ssh-keygen -q -s keys/user_ca -I carol -n carol -V -1d:+365d keys/id_carol.pub
# The certificate server's host key is trusted through its CA.
printf '@cert-authority [127.0.0.1]:2205 %s\n' "$(cat keys/host_ca.pub)" > keys/known_hosts

docker compose up -d --build
