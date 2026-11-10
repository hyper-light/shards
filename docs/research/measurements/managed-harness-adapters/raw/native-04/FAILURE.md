# Original driver failure

The first reusable-driver invocation failed1 before any native CLI launch:

`lockf -t 0 /tmp/g121-mac.lock python3 docs/research/measurements/managed-harness-adapters/native-probe.py --out docs/research/measurements/managed-harness-adapters/raw/native-04 --deadline-seconds 30`

The original traceback remains in the Kiln terminal tool transcript. Its error
was `AttributeError: module 'hashlib' has no attribute 'file_digest'` at line49.
The exact failing driver is retained as `failed-driver.py`, along with its
already-created disposable empty directories. This record is a transcription of
the tool failure, not a claim that a separate original stderr file was captured.
No CLI, account or provider request ran. The corrected driver streams the same
SHA256 calculation using the host's older supported hashlib API. New results use
native-05, preserving native-04.
