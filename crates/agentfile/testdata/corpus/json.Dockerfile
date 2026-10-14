FROM alpine
CMD ["a", "b\u00e9", "\ud800x", "\t\n"]
ENTRYPOINT [ "x" ]  
RUN ["unterminated
RUN ["a"] trailing
RUN []
RUN ["a",]
RUN {"a": "b"}
RUN ["\x41"]
RUN ["a"	,
"b"]
RUN  [ "\/" ,"\u0041\uDC00"]
SHELL ["/bin/bash","-c"]
VOLUME ["/a", "/b"]
VOLUME /c /d
