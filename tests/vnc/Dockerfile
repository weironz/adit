FROM ubuntu:24.04
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends openssh-server xvfb x11vnc tigervnc-standalone-server tigervnc-tools x11-utils x11-xserver-utils xdotool xauth xterm xfonts-base python3 python3-tk iproute2 ca-certificates curl && rm -rf /var/lib/apt/lists/*
RUN useradd -m -s /bin/bash adit-test && echo 'adit-test:adit-test-isolated' | chpasswd && mkdir -p /run/sshd && ssh-keygen -A
# Official release asset and GitHub-published SHA-256; downloaded only in the disposable image.
RUN curl -fsSL https://github.com/TurboVNC/turbovnc/releases/download/3.3.1/turbovnc_3.3.1_amd64.deb -o /tmp/turbovnc.deb && echo '5d99050312360a07c28aca484b944816aa32e9fe07912e3ed50e6bfda45f80ad  /tmp/turbovnc.deb' | sha256sum -c - && dpkg-deb --extract /tmp/turbovnc.deb / && rm /tmp/turbovnc.deb
COPY run.sh /run-vnc-tests.sh
COPY start-matrix.sh /start-matrix.sh
COPY load.py /load.py
COPY start-tls.sh /start-tls.sh
COPY start-resize.sh /start-resize.sh
RUN chmod +x /run-vnc-tests.sh
ENTRYPOINT ["/run-vnc-tests.sh"]
