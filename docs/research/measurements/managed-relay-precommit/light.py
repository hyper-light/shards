#!/usr/bin/env python3
"""Reuse private pure/light harnesses; never executes sockets, namespaces or guests."""
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
parent = subprocess.check_output(['git', 'show', parent_revision+':crates/shards/tests/managed_network.rs'])
module = root/'crates/shards/tests/managed_network/cancel.rs'
module_copy = proof/'cancel.rs'
module_copy.write_bytes(module.read_bytes())
host_source = parent + ('\\n#[path = '+json.dumps(str(module_copy))+']\\nmod cancel;\\n').encode()
(proof/'composition.json').write_text(json.dumps({'parent_revision':parent_revision,'parent_sha256':hashlib.sha256(parent).hexdigest(),'cancel_sha256':hashlib.sha256(module_copy.read_bytes()).hexdigest(),'note':'compile composition only; tracked parent not edited'})+'\\n')""")
    filters = ['cancel::precommit_record_rejects_an_actual_upstream_send', 'cancel::cancellation_record_rejects_work_expiry_and_preserves_sent_fact']
    commands = ''.join(" ['cargo','+1.98.1','test','--offline','--manifest-path',str(proof/'Cargo.toml'),'--lib',"+repr(name)+",'--','--exact'],\n" for name in filters)
    source = source.replace("commands=[", "commands=[\n"+commands)
else:
    raise SystemExit('mode must be guest or host')
exec(compile(source, str(base/('light.py' if mode == 'guest' else 'light-host.py')), 'exec'))
