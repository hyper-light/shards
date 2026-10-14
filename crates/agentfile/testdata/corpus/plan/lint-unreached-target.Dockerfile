FROM alpine AS unused
RUN echo $NOPE
COPY . $ALSO
FROM alpine AS Other
CMD echo hi
FROM alpine
CMD ["a"]
