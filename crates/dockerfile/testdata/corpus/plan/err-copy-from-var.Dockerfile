FROM alpine
ARG S=x
COPY --from=$S /a /a
