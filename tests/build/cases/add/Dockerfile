# syntax=docker/dockerfile:1.27.0@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
ARG BUSYBOX=docker.io/library/busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
FROM ${BUSYBOX}
ADD tree.tar.gz /unpacked/
ADD tree.tar.bz2 /unpacked/
ADD tree.tar.xz /unpacked/
ADD plain.txt /plain/
ADD --chown=7:8 --chmod=0600 plain.txt /owned.txt
