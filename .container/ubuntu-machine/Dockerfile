FROM ubuntu:24.04

ENV container=container

RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y \
        dbus \
        systemd \
        openssh-server \
        net-tools \
        iproute2 \
        iputils-ping \
        curl \
        wget \
        unzip \
        xz-utils \
        vim-tiny \
        man \
        sudo \
        ca-certificates \
        build-essential \
        pkg-config \
        libssl-dev && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/* && \
    yes | unminimize

RUN > /etc/machine-id
RUN > /var/lib/dbus/machine-id

RUN systemctl set-default multi-user.target

RUN systemctl mask \
    dev-hugepages.mount \
    sys-fs-fuse-connections.mount \
    systemd-update-utmp.service \
    systemd-tmpfiles-setup.service \
    console-getty.service

RUN systemctl disable \
    networkd-dispatcher.service

RUN sed -i -e 's/^AcceptEnv LANG LC_\*$/#AcceptEnv LANG LC_*/' \
    /etc/ssh/sshd_config
