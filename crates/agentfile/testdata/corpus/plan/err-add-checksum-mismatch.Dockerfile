FROM alpine
ADD --checksum=aaa https://example.com/r.git?checksum=bbb /r
