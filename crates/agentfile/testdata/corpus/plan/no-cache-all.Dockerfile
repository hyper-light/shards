FROM alpine AS build
RUN echo built > /out
COPY . /src
COPY --link a.txt /linked

FROM alpine AS Other
RUN echo other

FROM alpine
COPY --from=build /out /out
COPY --from=other /etc/os-release /os
RUN echo final
