cargo test --locked -j4 --profile test-release -p shards --test boot rejects_malformed_vm_arguments -- --exact --nocapture --test-threads=1
