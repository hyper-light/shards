# The sustained-I/O benchmark's image (images.sh): Debian trixie with Rust and Node.js, the
# two trees its workloads build (npm/cli, rust-analyzer), the npm cache their offline
# install reads, fio, and the guest's scripts. Its context is the benchmark's directory (fetch.sh), as images.sh
# stages it.
FROM mirror.gcr.io/library/node@sha256:7f486cabe892bff8fef04c0f7da8dba84a560be3e4e2740b7a86afc57ebec315 AS node
FROM mirror.gcr.io/library/rust@sha256:4df2ae8cfdfebdfc13a65da7f253b73d68983dc0339f2719bc17a279c913b13d
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm
COPY fio /usr/local/bin/fio
COPY npm-cache /root/.npm
COPY cli /work/cli
COPY rust-analyzer /work/rust-analyzer
COPY sio /sio
