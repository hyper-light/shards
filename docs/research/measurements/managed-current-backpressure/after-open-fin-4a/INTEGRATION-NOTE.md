# Later Capturing variant is a separate integration gate

ROOT reports allocation-backed capture candidate7fe introduces explicit remote
State::Capturing for a buffer lent to the trusted capture caller. Actual matrix
r932 failed E0004 in Worker::ready at its first Linux target, before any later
target/native pass. Its original logs remain ROOT-owned. This is a reproduced
integration compilation failure, not a new runtime/actor bug.

ROOT will compose an explicit unexpected-Capturing failure arm, preserving the
held-record/replay assertions. A loan must not be treated as Ready or terminal.

This current campaign remains exact older source
`9764144d5a7170c1ba6607ecd0222a0015db12de`, whose remote State has the original
four variants. No rebase, source edit, rerun or later-target passing claim follows
from that notification. The eventual composed source needs its own ROOT gates.

The current private r171 preflight instead stopped on installed Zig0.17 versus
repository-pinned0.16 before any Rust compilation or native test. That distinct
tool mismatch is preserved; it is not E0004 or a product/case failure.
