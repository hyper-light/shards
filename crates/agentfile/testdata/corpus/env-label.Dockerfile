FROM alpine
ENV A=1 B="two words" C='3'
ENV legacy value with spaces
LABEL a=b "c d"=e
ARG X Y=1 Z=""
ARG
ENV legacy2	  tabbed value
