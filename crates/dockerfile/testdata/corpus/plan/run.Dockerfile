FROM alpine
RUN echo one
RUN ["echo", "two"]
ENV X=1
RUN echo $X
