# SSH server for the tunnel tests. User tunnel, password Honcho_test1, and
# the two test keys next to this file. Test-only credentials.
FROM alpine:3.20
RUN apk add --no-cache openssh \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh tunnel \
 && echo 'tunnel:Honcho_test1' | chpasswd \
 && mkdir -p /etc/ssh/sshd_config.d \
 && printf 'PasswordAuthentication yes\nAllowTcpForwarding yes\nPermitRootLogin no\n' \
    > /etc/ssh/sshd_config.d/tunnel.conf
COPY --chown=tunnel:tunnel --chmod=600 authorized_keys /home/tunnel/.ssh/authorized_keys
RUN chmod 700 /home/tunnel/.ssh
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
