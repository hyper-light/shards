FROM alpine as Build
CMD echo a
CMD echo b
ENTRYPOINT echo e
ENV PASSWORD=x
ARG SECRET_TOKEN
WORKDIR rel
run echo lower
RUN echo $UNDEFINED
EXPOSE 80/TCP 1.2.3.4:80:80
