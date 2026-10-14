FROM alpine
HEALTHCHECK --interval=5s CMD ["curl", "x"]
HEALTHCHECK NONE
HEALTHCHECK CMD curl -f http://localhost/
