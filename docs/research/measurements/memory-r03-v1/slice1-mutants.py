import sys
import os; root=os.path.join(os.path.dirname(os.path.abspath(__file__)), '../../../../')
muts=[
 ('crates/vmm/src/devices/virtio/fs/headroom.rs',
  'now.saturating_duration_since(self.at) >= shards_abi::pace::pause(self.took, right, Duration::MAX)',
  'now.saturating_duration_since(self.at) >= { let _ = right; self.took.saturating_mul(100) }'),
 ('crates/vmm/src/platform/pressure.rs',
  'acted.map_or(1, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX).clamp(1, WINDOW_US))',
  '{ let _ = acted; 140_000 }'),
 ('crates/vmm/src/platform/pressure.rs',
  'kv.strip_prefix("total=")', 'kv.strip_prefix("avg10=")'),
 ('crates/vmm/src/platform/pressure.rs',
  'shards_abi::pace::pause(acted.unwrap_or(look), raised, Duration::MAX)',
  '{ let _ = (acted, look, raised); Duration::from_secs(2) }'),
 ('crates/shards/src/daemon/budget.rs',
  'let pace = shards_abi::pace::pause(took, at.saturating_duration_since(since), Duration::MAX);',
  'let pace = { let _ = (took, since); Duration::from_secs(2) };'),
 ('crates/shards/src/daemon/groups.rs',
  'at.checked_add(shards_abi::pace::pause(self.took, unchanged, Duration::MAX))',
  'at.checked_add({ let _ = unchanged; Duration::from_secs(2) })'),
 ('crates/shards/src/build/give.rs',
  'if let Some(t) = took.filter(|t| !t.is_zero()) {',
  'if let Some(t) = took.filter(|_| false) {'),
 ('crates/shards/src/pressure.rs',
  '(Some(last), Some(was)) => now.saturating_duration_since(last) <= was.saturating_mul(2),',
  '(Some(last), Some(_)) => now.saturating_duration_since(last) <= Duration::from_secs(4),'),
 ('crates/shards/src/build/builder.rs',
  'shards_vmm::platform::wait_readable(&[self.conn.raw_socket()], Some(left))',
  'shards_vmm::platform::wait_readable(&[self.conn.raw_socket()], Some(left.max(Duration::from_millis(1))))'),
 ('crates/init/src/plug.rs',
  '    stays > began_with\n', '    let _ = (began_with, stays);\n    false\n'),
 ('crates/init/src/plug.rs',
  '    shards_abi::pace::pause(look, waited, Duration::MAX)\n',
  '    let _ = (look, waited);\n    Duration::from_millis(1)\n'),
 ('crates/init/src/plug.rs',
  '''                if !lifted {
                    g.lift();
                    lifted = true;
                }''','''                if lifted {
                    return Came::Broken(came);
                }
                if !lifted {
                    g.lift();
                    lifted = true;
                }'''),
]
for f,a,b in muts:
    p=root+f; s=open(p).read()
    assert s.count(a)==1, (f,a[:60],s.count(a))
    open(p,'w').write(s.replace(a,b))
print(len(muts),'mutants applied')
