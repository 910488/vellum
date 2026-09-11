# A disposable stand-in for a real Linux deployment target.
#
# Remote Manager refuses to deploy unless the host offers three things:
# Docker, user systemd, and enabled lingering. A plain container offers none
# of them, which is why the dev loop used to need a real machine. This image
# provides all three -- systemd as PID 1, its own dockerd (not the build
# host's socket, so published proxy ports land in this host's own network
# namespace), and a login user whose session survives SSH disconnect.
#
# It is a test fixture, not a shipped artifact: it is deliberately not pinned
# by digest, and `scripts/dev-host.ps1 reset` throws each instance away.
FROM debian:bookworm

ENV DEBIAN_FRONTEND=noninteractive
ENV container=docker

RUN apt-get update && apt-get install -y --no-install-recommends \
      systemd systemd-sysv dbus dbus-user-session \
      openssh-server sudo ca-certificates curl gnupg \
      iproute2 iptables procps uidmap \
    && install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian bookworm stable" \
       > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends docker-ce docker-ce-cli containerd.io \
    && rm -rf /var/lib/apt/lists/*

# Units that cannot work inside a container. Left unmasked they fail at boot
# and `systemctl is-system-running` reports `degraded`, which is exactly the
# signal the dev loop uses to decide the host came up clean.
RUN systemctl mask \
      systemd-udevd systemd-udev-trigger systemd-modules-load \
      systemd-journald-audit.socket \
      sys-kernel-config.mount sys-kernel-debug.mount \
    && systemctl enable ssh docker

# uid 1000 so /run/user/1000 matches what a first login user gets on a real
# host, which is the path the agent's transient user units are addressed by.
RUN useradd --create-home --uid 1000 --shell /bin/bash --groups sudo,docker vellum \
    && echo 'vellum ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/vellum \
    && chmod 0440 /etc/sudoers.d/vellum \
    && install -d -m 700 -o vellum -g vellum /home/vellum/.ssh \
    && mkdir -p /run/sshd

STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
