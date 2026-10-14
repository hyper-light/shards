# check=skip=UndefinedVar,JSONArgsRecommended
FROM alpine
RUN echo $NOPE
CMD echo x
# check=skip=all
RUN echo $ALSO_NOPE
