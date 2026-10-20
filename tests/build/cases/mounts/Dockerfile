# syntax=docker/dockerfile:1.27.0@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
ARG BUSYBOX=docker.io/library/busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
FROM ${BUSYBOX}
RUN --mount=type=cache,target=/cache echo x > /cache/x && ls /cache > /cache-listing
RUN --mount=type=bind,source=files,target=/src cp -r /src /from-bind
RUN --mount=type=tmpfs,target=/scratch touch /scratch/gone && ls /scratch > /scratch-listing
RUN --mount=type=secret,id=token wc -c < /run/secrets/token > /secret-size
RUN --mount=type=secret,id=token,env=TOKEN test -n "$TOKEN" && echo set > /secret-env
