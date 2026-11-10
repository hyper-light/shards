timeout --signal=TERM --kill-after=30s 3600s cargo test --locked -j4 --profile test-release -p shards --test managed_output --no-run --message-format=json
