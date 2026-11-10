#!/usr/bin/env python3
"""Reuse source-linked pure checks; actual VM/event/socket execution stays ignored."""
from pathlib import Path
import sys

mode = sys.argv.pop(1)
root = Path.cwd()
base = root / 'docs/research/measurements/managed-network-sock-addr'
if mode == 'guest':
    source = (base / 'light.py').read_text().replace("'-j4'", "'-j1'")
elif mode == 'host':
    source = (base / 'light-host.py').read_text().replace("'-j4'", "'-j1'")
    source = source.replace("actual.read_bytes()", "host_source")
    source = source.replace("actual=root/'crates/shards/tests/managed_network.rs'", """actual=root/'crates/shards/tests/managed_network.rs'
parent_revision = 'e1637959af1dbae631aa6f91c0d4d227f4c7dfac'
supplied_parent = os.environ.get('SHARDS_LIGHT_HOST_PARENT')
parent = Path(supplied_parent).read_bytes() if supplied_parent else subprocess.check_output(['git', 'show', parent_revision+':crates/shards/tests/managed_network.rs'])
module = root/'crates/shards/tests/managed_network/cancel.rs'
module_copy = proof/'cancel.rs'
module_copy.write_bytes(module.read_bytes())
host_source = parent + ('\\n#[path = '+json.dumps(str(module_copy))+']\\nmod cancel;\\n').encode()
(proof/'composition.json').write_text(json.dumps({'parent_revision':parent_revision,'parent_sha256':hashlib.sha256(parent).hexdigest(),'cancel_sha256':hashlib.sha256(module_copy.read_bytes()).hexdigest(),'note':'copied compile composition with inert common stubs; tracked parent unchanged'})+'\\n')""")
    filters = [
        'cancel::notification_expiry_record_cannot_claim_cancellation_or_hide_a_sent_datagram',
        'cancel::unexpected_work_record_retains_consumed_extra_and_rejects_expiry_or_cancel',
        'cancel::backend_work_record_rejects_cancel_expiry_and_unjoined_or_missing_receipts',
        'cancel::precommit_record_rejects_an_actual_upstream_send',
        'cancel::cancellation_record_rejects_work_expiry_and_preserves_sent_fact',
    ]
    commands = ''.join(" ['cargo','+1.98.1','test','--offline','--manifest-path',str(proof/'Cargo.toml'),'--lib',"+repr(name)+",'--','--exact'],\n" for name in filters)
    source = source.replace('commands=[', 'commands=[\n'+commands)
else:
    raise SystemExit('mode must be guest or host')
exec(compile(source, str(base/('light.py' if mode == 'guest' else 'light-host.py')), 'exec'))
