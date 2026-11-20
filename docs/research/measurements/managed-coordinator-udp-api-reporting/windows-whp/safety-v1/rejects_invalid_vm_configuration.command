cargo test --locked -j4 --profile test-release -p shards --test boot rejects_invalid_vm_configuration -- --exact --nocapture --test-threads=1
