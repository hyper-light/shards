FROM alpine AS build
RUN make
FROM scratch AS out
COPY --from=build /app /app
CMD ["/app"]
