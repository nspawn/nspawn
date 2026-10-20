# syntax=docker/dockerfile:1.27.0@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
ARG BUSYBOX=docker.io/library/busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
FROM ${BUSYBOX}
WORKDIR /app
COPY files/ ./
COPY --chmod=0750 run.sh /opt/bin/
COPY --chown=1000:1000 owned.txt /app/owned/
RUN echo "made" > /app/made && ln -s made /app/link && ln /app/made /app/hard && chmod 4755 /app/made
RUN rm /bin/vi /app/top.txt && mkdir -p /etc/opaque && echo a > /etc/opaque/a && echo changed >> /etc/hostname
RUN rm -rf /etc/opaque && mkdir /etc/opaque && echo b > /etc/opaque/b
RUN true
RUN <<EOT
set -e
echo heredoc > /app/heredoc
mkfifo /app/fifo
EOT
COPY <<EOT /app/inline.txt
inline $HOSTNAME
EOT
