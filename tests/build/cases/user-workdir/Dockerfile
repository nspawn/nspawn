# syntax=docker/dockerfile:1.27.0@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
ARG BUSYBOX=docker.io/library/busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
FROM ${BUSYBOX}
RUN adduser -D -u 1234 app
USER app
WORKDIR /home/app/work
RUN echo mine > mine && id -u > uid
USER root:root
WORKDIR sub
RUN pwd > /where
