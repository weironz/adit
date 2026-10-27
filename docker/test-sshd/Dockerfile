# A tiny throwaway OpenSSH server for Adit's integration tests. It enables
# everything the test matrix needs (password auth + TCP forwarding + SFTP) and
# is NOT meant for any real use — the credentials are public on purpose.
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends openssh-server openssh-client net-tools netcat-openbsd gcc libc6-dev libpam0g-dev \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /run/sshd /usr/local/lib/security \
 && ssh-keygen -A \
 && useradd -m -s /bin/sh adit \
 && echo 'adit:aditpw' | chpasswd

COPY sshd_config /etc/ssh/sshd_config
COPY pam_adit_fixture.c /tmp/pam_adit_fixture.c
RUN gcc -Wall -Wextra -Werror -shared -fPIC /tmp/pam_adit_fixture.c -o /usr/local/lib/security/pam_adit_fixture.so -lpam \
 && rm /tmp/pam_adit_fixture.c \
 && printf 'auth required pam_unix.so\nauth required /usr/local/lib/security/pam_adit_fixture.so\naccount required pam_permit.so\nsession required pam_permit.so\n' > /etc/pam.d/sshd \
 && printf 'ADIT-FIXTURE-CODE\n' > /etc/adit-fixture-code

EXPOSE 2222
CMD ["/usr/sbin/sshd", "-D", "-e"]
