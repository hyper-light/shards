timeout --signal=TERM --kill-after=30s 1800s cargo build --locked -j4 -p shards-testguest --profile guest --target x86_64-unknown-linux-musl --target-dir source/target/guest
