from pathlib import Path
import os,json,subprocess,hashlib,difflib
r=Path.home()/'root-managed-presentation-hook-mutations-e1bf-v1';p=Path('crates/testguest/src/managed_network/intercept/sock_addr.rs');original=p.read_bytes();assert hashlib.sha256(original).hexdigest()=='7c83dc31708476f93c727190e7dd7b7b6d0ccf412c4b6ae584051a4b1a7974c9'
binary=json.loads((Path.home()/'root-managed-presentation-e1bf-v1/test-artifact.json').read_text())['path'];base=json.loads((Path.home()/'root-managed-presentation-e1bf-v1/connected-fixed-original-peer/environment.json').read_text());env=os.environ.copy();env.update(base);env['CARGO_TARGET_DIR']=str(Path.home()/'managed-capture-target');env['SHARDS_REQUIRE_VMS']='1';env.pop('SHARDS_HELPERS',None)
(r/'source.json').write_text(json.dumps({'head':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'original_sha256':hashlib.sha256(original).hexdigest(),'test_binary':binary},indent=2)+'\n')
connected=b'(Self::FixedOriginalPeer, Mode::Connected) => &[Connect4, Recvmsg4, Getpeername4]';unconnected=b'(Self::FixedOriginalPeer, Mode::Unconnected) => &[Sendmsg4, Recvmsg4]'
cases=[('Connect4','connected',connected,b'(Self::FixedOriginalPeer, Mode::Connected) => &[Recvmsg4, Getpeername4]'),('Sendmsg4','unconnected',unconnected,b'(Self::FixedOriginalPeer, Mode::Unconnected) => &[Recvmsg4]'),('Recvmsg4-connected','connected',connected,b'(Self::FixedOriginalPeer, Mode::Connected) => &[Connect4, Getpeername4]'),('Recvmsg4-unconnected','unconnected',unconnected,b'(Self::FixedOriginalPeer, Mode::Unconnected) => &[Sendmsg4]'),('Getpeername4','connected',connected,b'(Self::FixedOriginalPeer, Mode::Connected) => &[Connect4, Recvmsg4]')]
cmd=[binary,'direct_guest_socket_address_udp4_requires_original_native_tuple','--exact','--ignored','--nocapture','--test-threads=1'];(r/'command.json').write_text(json.dumps(cmd)+'\n');results=[]
def run(label,mode,guard):
 d=r/label;d.mkdir();settings=base|{'SHARDS_SOCK_ADDR_MODE':mode,'SHARDS_NETWORK_RECORDS':str(d)};runenv=env|settings;(d/'environment.json').write_text(json.dumps(settings,indent=2)+'\n')
 with (d/'test.log').open('wb') as out:child=subprocess.run(cmd,env=runenv,stdout=out,stderr=subprocess.STDOUT)
 (d/'test.exit').write_text(str(child.returncode)+'\n');records=[]
 for line in (d/f'sock-addr-udp4-{mode}-fixed-original-peer.stdout').read_text(errors='replace').splitlines():
  if line.startswith(('managed-sock-addr ','managed-intercept ')):records.append(json.loads(line.split(' ',1)[1]))
 (d/'observed.json').write_text(json.dumps(records,indent=2)+'\n')
 target=next(i for i in records if i['type']=='Target');identities=[i for i in records if i['type']=='ClientIdentity'];arrivals=[i for i in records if i['type']=='UdpArrival'];cleanup=next(i for i in records if i['type']=='Cleanup' and i['state']=='Observed');reaped=[i for i in records if i['type']=='ChildReaped']
 assert all(cleanup[k] for k in ['backend_joined','relay_joined','link_detached','cgroup_removed']),cleanup
 assert len(reaped)==2 and all(i['reaped'] for i in reaped),reaped
 if guard is None:
  assert child.returncode==0
  witness='strict exact original endpoint, data and owned cleanup passed'
 else:
  assert child.returncode!=0
  if guard in ['Connect4','Sendmsg4']:
   extra=[i for i in arrivals if i['role']=='Backend' and i['ordinal']>2];assert extra,'actual additional backend effect required, not merely missing hook record'
   witness={'actual_additional_backend_arrivals':extra}
  elif guard.startswith('Recvmsg4'):
   leaked=[i for i in arrivals if i['role']=='SockAddrHookClient' and i['peer']!=target['backend']];assert leaked,'actual returned peer leak required'
   if mode=='connected':assert identities[-1]['peer']==target['backend'],'getpeername guard should still hold'
   witness={'actual_receive_peer_leak':leaked}
  else:
   assert identities[-1]['peer']!=target['backend'],'actual getpeername mismatch required'
   kept=[i for i in arrivals if i['role']=='SockAddrHookClient'];assert len(kept)==2 and all(i['peer']==target['backend'] for i in kept),'receive presentation should still hold'
   witness={'actual_getpeername':identities[-1],'receive_peers_still_original':True}
 result={'label':label,'mode':mode,'actual_exit':child.returncode,'cleanup':cleanup,'witness':witness};results.append(result);(r/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(label,'actual exit',child.returncode,'actual witness checked; cleanup complete',flush=True)
for name,mode,needle,replacement in cases:
 assert original.count(needle)==1;mutant=original.replace(needle,replacement,1);(r/(name+'.patch')).write_text(''.join(difflib.unified_diff(original.decode().splitlines(True),mutant.decode().splitlines(True),fromfile=str(p),tofile=str(p))))
 try:
  p.write_bytes(mutant);p.touch();run('removed-'+name,mode,name)
 finally:
  p.write_bytes(original);p.touch();assert p.read_bytes()==original
  (r/'restored.sha256').write_text(hashlib.sha256(p.read_bytes()).hexdigest()+'\n');(r/'restored.diff').write_bytes(subprocess.check_output(['git','diff','--',str(p)]))
 run('restored-'+name,mode,None)
