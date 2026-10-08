FROM alpine AS unreached
RUN --mount=type=secret,id=a,required=maybe true
FROM alpine
CMD ["a"]
