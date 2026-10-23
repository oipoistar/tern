# Tern's SSH lab: one image, a role per container (see compose.yaml). Only
# Debian's own packages.
FROM debian:bookworm-slim
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      openssh-server openssh-client libpam-google-authenticator oathtool \
      dante-server tinyproxy python3 curl netcat-openbsd procps iproute2 tmux \
 && rm -rf /var/lib/apt/lists/* \
 # Each container makes its own host keys when it first starts, so no two
 # servers share one.
 && rm -f /etc/ssh/ssh_host_*
COPY entrypoint.sh /entrypoint.sh
# A docker that lists one container, for Tern's container checks.
COPY docker-stand-in.sh /usr/local/bin/docker
# A checkout on Windows may have given the scripts CRLF line endings.
RUN sed -i 's/\r$//' /entrypoint.sh /usr/local/bin/docker \
 && chmod +x /entrypoint.sh /usr/local/bin/docker
ENTRYPOINT ["/entrypoint.sh"]
