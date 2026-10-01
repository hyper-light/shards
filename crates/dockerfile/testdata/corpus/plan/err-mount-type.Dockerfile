FROM alpine
ARG T=bnd
RUN --mount=type=$T,target=/x ls
