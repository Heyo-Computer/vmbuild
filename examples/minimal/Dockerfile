# The smallest Firecracker-bootable rootfs worth building: a Debian userland
# with an init that brings up the network and sshd. Everything the example
# scripts need and nothing else, so a cold build is seconds and the image is
# ~200 MB. See ../fastcar for what a production one looks like.
#
# Only the filesystem survives into the ext4: ENV/CMD/ENTRYPOINT/USER are
# image config and are dropped. The kernel's init=/init.sh is the entry point.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates iproute2 openssh-server \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /run/sshd \
    && echo "PermitRootLogin yes" >> /etc/ssh/sshd_config \
    && echo "PermitEmptyPasswords yes" >> /etc/ssh/sshd_config \
    && passwd -d root \
    && ssh-keygen -A
COPY init.sh /init.sh
RUN chmod +x /init.sh
