# Paths a step's setup resolves, as BuildKit and runc resolve them, inside the step's
# root: a mount target an earlier step made an absolute symlink, and a working directory
# an earlier step replaced with one. Resolved from outside the root, either reaches the
# builder's own files.
FROM alpine:3.22
RUN mkdir /inside && ln -s /inside /target
RUN --mount=type=tmpfs,target=/target touch /inside/x
WORKDIR /wd/sub
RUN rm -rf /wd && ln -s /inside2 /wd
RUN true
