# syntax=docker/dockerfile:1.27.0@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
ARG BUSYBOX=docker.io/library/busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
ARG ALPINE=docker.io/library/alpine:3.22@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8
FROM ${BUSYBOX} AS build
RUN mkdir -p /out/dir && echo built > /out/artifact && chmod 0640 /out/artifact && echo inner > /out/dir/inner
FROM build AS unused
RUN exit 1
FROM ${ALPINE}
COPY --from=build /out/artifact /artifact
COPY --from=0 /out/dir /copied/
COPY --from=build /out /whole
CMD ["cat", "/artifact"]
