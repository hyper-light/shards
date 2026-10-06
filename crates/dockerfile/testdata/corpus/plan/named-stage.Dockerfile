FROM alpine AS deps
RUN touch /a
FROM scratch
COPY --from=deps /a /a
