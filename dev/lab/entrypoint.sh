#!/bin/bash
# Sets a lab container up for its role, then runs its server in the
# foreground. Every SSH server has the user `lab` (password `lab-pass`, the
# lab's keys authorized) unless its role says otherwise.
set -euo pipefail
role=${1:?which role}
keys=/lab/keys

user() {
  id "$1" >/dev/null 2>&1 || useradd -m -s /bin/bash "$1"
  echo "$1:lab-pass" | chpasswd
  install -d -m 700 -o "$1" -g "$1" "/home/$1/.ssh"
}

authorize() {
  cat "$keys"/id_ed25519.pub "$keys"/id_ecdsa.pub "$keys"/id_rsa.pub "$keys"/id_locked.pub \
    > "/home/$1/.ssh/authorized_keys"
  chown "$1:$1" "/home/$1/.ssh/authorized_keys"
  chmod 600 "/home/$1/.ssh/authorized_keys"
}

# Runs sshd with the lab's common settings and the role's, from stdin. The
# drop-in comes before Debian's defaults, so its values win.
sshd() {
  mkdir -p /run/sshd
  ssh-keygen -A >/dev/null
  {
    echo "AcceptEnv LANG LC_* TERN_*"
    echo "AllowTcpForwarding yes"
    echo "AllowAgentForwarding yes"
    cat
  } > /etc/ssh/sshd_config.d/lab.conf
  exec /usr/sbin/sshd -D -e
}

# A one-time code for `lab`, from a fixed secret, so tests can work it out.
one_time_codes() {
  printf '%s\n' JBSWY3DPEHPK3PXP '" RATE_LIMIT 30 30' '" WINDOW_SIZE 3' '" TOTP_AUTH' \
    > /home/lab/.google_authenticator
  chown lab:lab /home/lab/.google_authenticator
  chmod 400 /home/lab/.google_authenticator
}

case "$role" in
  password)
    user lab
    sshd <<'EOF'
PasswordAuthentication yes
KbdInteractiveAuthentication no
PubkeyAuthentication no
EOF
    ;;
  key | deep)
    user lab
    authorize lab
    sshd <<'EOF'
PasswordAuthentication no
KbdInteractiveAuthentication no
EOF
    ;;
  otp)
    # The account's password, then a one-time code, as questions.
    user lab
    one_time_codes
    echo 'auth required pam_google_authenticator.so' >> /etc/pam.d/sshd
    sshd <<'EOF'
UsePAM yes
PasswordAuthentication no
PubkeyAuthentication no
KbdInteractiveAuthentication yes
AuthenticationMethods keyboard-interactive
EOF
    ;;
  mfa)
    # A key, then only a one-time code.
    user lab
    authorize lab
    one_time_codes
    sed -i 's/^@include common-auth/#&/' /etc/pam.d/sshd
    echo 'auth required pam_google_authenticator.so' >> /etc/pam.d/sshd
    sshd <<'EOF'
UsePAM yes
PasswordAuthentication no
KbdInteractiveAuthentication yes
AuthenticationMethods publickey,keyboard-interactive
EOF
    ;;
  cert)
    # A host certificate from the lab's host CA, and users signed by its
    # user CA: `carol` has no authorized keys at all.
    user carol
    ssh-keygen -A >/dev/null
    install -m 600 "$keys/host_ca" /tmp/host_ca
    ssh-keygen -q -s /tmp/host_ca -I lab-cert-host -h -n 127.0.0.1,localhost,cert \
      -V -1d:+365d /etc/ssh/ssh_host_ed25519_key.pub
    rm /tmp/host_ca
    install -m 644 "$keys/user_ca.pub" /etc/ssh/user_ca.pub
    sshd <<'EOF'
PasswordAuthentication no
KbdInteractiveAuthentication no
HostCertificate /etc/ssh/ssh_host_ed25519_key-cert.pub
TrustedUserCAKeys /etc/ssh/user_ca.pub
EOF
    ;;
  bastion)
    # Keys or a plain password; no questions.
    user lab
    authorize lab
    sshd <<'EOF'
PasswordAuthentication yes
KbdInteractiveAuthentication no
EOF
    ;;
  hop2)
    # Keys, or the password asked as a question.
    user lab
    authorize lab
    sshd <<'EOF'
UsePAM yes
PasswordAuthentication no
KbdInteractiveAuthentication yes
EOF
    ;;
  strict)
    # Only today's algorithms.
    user lab
    authorize lab
    sshd <<'EOF'
PasswordAuthentication no
KbdInteractiveAuthentication no
KexAlgorithms sntrup761x25519-sha512@openssh.com,curve25519-sha256,curve25519-sha256@libssh.org
Ciphers chacha20-poly1305@openssh.com,aes256-gcm@openssh.com
MACs hmac-sha2-512-etm@openssh.com,hmac-sha2-256-etm@openssh.com
HostKeyAlgorithms ssh-ed25519
PubkeyAcceptedAlgorithms ssh-ed25519
EOF
    ;;
  legacy)
    # What older servers offer: an RSA host key, finite-field key exchange,
    # CTR ciphers and SHA-1 MACs among the rest.
    user lab
    authorize lab
    sshd <<'EOF'
PasswordAuthentication yes
KbdInteractiveAuthentication no
HostKey /etc/ssh/ssh_host_rsa_key
KexAlgorithms diffie-hellman-group14-sha256,diffie-hellman-group14-sha1,diffie-hellman-group-exchange-sha256
Ciphers aes128-ctr,aes256-ctr
MACs hmac-sha2-256,hmac-sha1
EOF
    ;;
  socks)
    cat > /etc/danted.conf <<'EOF'
logoutput: stderr
internal: 0.0.0.0 port = 1080
# Out through whichever network holds the destination.
external: eth0
external: eth1
external.rotation: route
clientmethod: none
socksmethod: none
user.privileged: root
user.unprivileged: nobody
client pass { from: 0.0.0.0/0 to: 0.0.0.0/0 }
socks pass { from: 0.0.0.0/0 to: 0.0.0.0/0 command: connect }
EOF
    exec danted -f /etc/danted.conf
    ;;
  httpproxy)
    cat > /etc/tinyproxy/tinyproxy.conf <<'EOF'
User nobody
Group nogroup
Port 8888
Listen 0.0.0.0
Timeout 600
Allow 0.0.0.0/0
ConnectPort 22
DisableViaHeader Yes
LogLevel Info
EOF
    exec tinyproxy -d -c /etc/tinyproxy/tinyproxy.conf
    ;;
  web)
    mkdir -p /srv
    echo "hello from the lab's web server" > /srv/hello.txt
    cd /srv
    exec python3 -m http.server 8000
    ;;
  *)
    echo "no such role: $role" >&2
    exit 2
    ;;
esac
