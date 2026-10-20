# syntax=docker/dockerfile:1.27.0@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
ARG BUSYBOX=docker.io/library/busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
FROM ${BUSYBOX}
COPY src /dir-as-is
COPY src/ /dir-slash/
COPY src/a/b/f.txt /newfile
COPY src/a/b/f.txt /newdir/
COPY src/*.txt /wild/
COPY src/a/b/f.txt src/c/g.txt /two/
COPY --parents src/a/b/f.txt src/c/g.txt /parents/
COPY --exclude=*.log src /excluded
COPY src/c /existing-file-dir/
