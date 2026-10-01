FROM userwd
RUN pwd && whoami
WORKDIR sub
COPY x .
CMD run
