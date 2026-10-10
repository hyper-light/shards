FROM curlimages/curl:8.11.1
USER root
RUN env | grep -i proxy | sort; echo ---; for f in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/ca-bundle.pem /etc/pki/tls/cacert.pem /etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem /etc/ssl/cert.pem /cacert.pem; do ls -la $f 2>&1; done; echo ---; grep -n "buildkit proxy CA" /etc/ssl/certs/ca-certificates.crt; tail -c 900 /etc/ssl/certs/ca-certificates.crt
RUN C="--cacert /etc/ssl/certs/ca-certificates.crt"; curl -sS $C http://172.18.0.2/hello; curl -sS $C -o /dev/null -w "redirect %{http_code}\n" http://172.18.0.2/redirect; curl -sS $C -L http://172.18.0.2/redirect; curl -sS $C -X POST -d x http://172.18.0.2/hello; curl -sS $C -r 0-1 http://172.18.0.2/hello; echo; curl -sS $C -D - -o /dev/null http://172.18.0.2/denied; curl -sS $C -D - -o /dev/null https://example.com/denied; curl -sS $C -o /dev/null -w "https %{http_code}\n" https://example.com/; curl -sS $C --max-time 5 -o /dev/null -w "refused %{http_code}\n" http://172.18.0.2:81/; echo done
RUN --network=none env | grep -i proxy || echo no-proxy-env
RUN --network=host env | grep -i proxy | sort | head -3
