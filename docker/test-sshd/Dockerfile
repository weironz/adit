# A tiny throwaway OpenSSH server for Adit's integration tests. It enables
# everything the test matrix needs (password auth + TCP forwarding + SFTP) and
# is NOT meant for any real use — the credentials are public on purpose.
FROM alpine:3.20

RUN apk add --no-cache openssh \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh adit \
 && echo 'adit:aditpw' | chpasswd

COPY sshd_config /etc/ssh/sshd_config

EXPOSE 2222
CMD ["/usr/sbin/sshd", "-D", "-e"]
