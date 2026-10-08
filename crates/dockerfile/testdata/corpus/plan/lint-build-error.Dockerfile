FROM alpine AS Base
COPY --from=missing /a /b
FROM base
RUN true
