# syntax=docker/dockerfile:1.27.0@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
ARG BUSYBOX=docker.io/library/busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
FROM ${BUSYBOX}
LABEL org.example.case="settings" org.example.multi="a b"
LABEL maintainer=nobody
ENV GREETING="hello world" PATH=/opt/bin:$PATH
ARG VERSION=1.2
ENV VERSION=${VERSION}
EXPOSE 8080/tcp 53/udp 9000-9002
VOLUME /data ["/logs"]
STOPSIGNAL SIGINT
HEALTHCHECK --interval=5s --timeout=3s --start-period=1s --retries=4 CMD test -f /etc/passwd
SHELL ["/bin/sh", "-ec"]
ENTRYPOINT ["/bin/echo"]
CMD ["serve", "$GREETING"]
