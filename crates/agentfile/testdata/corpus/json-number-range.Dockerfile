FROM alpine
CMD ["a", 1e400]
RUN ["b", [-1e309]]
SHELL ["d", 1.7976931348623159e308]
ENTRYPOINT ["e", {"k": 1e999}]
