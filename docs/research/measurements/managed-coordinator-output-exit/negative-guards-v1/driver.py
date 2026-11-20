from pathlib import Path
import subprocess,os,json,hashlib,difflib
r=Path.home()/'root-managed-exit-negative-guards-ebbd-v1';p=Path('docs/research/measurements/managed-output-cost/fixture.rs');original=p.read_bytes();env=os.environ.copy();env['CARGO_TARGET_DIR']=str(Path.home()/'managed-capture-target');env['SHARDS_REQUIRE_VMS']='1';env['SHARDS_TEST_KERNEL']=str(Path.home()/'managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy');env['SHARDS_OUTPUT_COST_SOURCE_REVISION']=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip();env.pop('SHARDS_HELPERS',None)
(r/'source.json').write_text(json.dumps({'head':env['SHARDS_OUTPUT_COST_SOURCE_REVISION'],'original_sha256':hashlib.sha256(original).hexdigest()},indent=2)+'\n');(r/'original.rs').write_bytes(original)
poison_old=b'    let daemon = named(&fixture.home.join(shards_ipc::PID));'
poison_new=b'''    let mut daemon = named(&fixture.home.join(shards_ipc::PID));
    if let Ok(identity) = &mut daemon {
        identity.start = identity.start.and_then(|start| start.checked_add(1));
    }'''
identity_guard=b'''        if observed != expected {
            return Err(ObservationFailure::DifferentIdentity { expected, observed });
        }'''
stop=b'    let stopped = fixture.shutdown(cleanup_until);';extra=stop+b'\n    let _extra_owned_fd = std::fs::File::open("/dev/null").unwrap();'
fd_guard=b'    assert!(fd_returned, "exit observation exact FD delta/return: {fd_record}");'
expired_old=b'            let (record, passed, fds) = proof.finish(cleanup_until);';expired_new=b'            let (record, passed, fds) = proof.finish(Instant::now());'
for needle in [poison_old,identity_guard,stop,fd_guard,expired_old]:assert original.count(needle)==1,needle
poison=original.replace(poison_old,poison_new,1);extra_fd=original.replace(stop,extra,1)
cases=[('wrong-identity-guarded',poison,'identity-rejected'),('wrong-identity-check-removed',poison.replace(identity_guard,b'        let _ = (observed, expected);',1),'identity-false-accept'),('extra-fd-guarded',extra_fd,'fd-rejected'),('extra-fd-check-removed',extra_fd.replace(fd_guard,b'    let _ = fd_returned;',1),'fd-false-accept'),('expired-exit-deadline',original.replace(expired_old,expired_new,1),'deadline-rejected')];results=[]
compile_cmd=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_output','--no-run','--message-format=json'];(r/'compile.command.json').write_text(json.dumps(compile_cmd)+'\n')
def run(name,source,expected):
 d=r/name;d.mkdir();p.write_bytes(source);p.touch();(d/'mutation.patch').write_text(''.join(difflib.unified_diff(original.decode().splitlines(True),source.decode().splitlines(True),fromfile=str(p),tofile=str(p))));(d/'source.sha256').write_text(hashlib.sha256(source).hexdigest()+'\n')
 with (d/'compile.jsonl').open('wb') as out,(d/'compile.log').open('wb') as err:c=subprocess.run(compile_cmd,env=env,stdout=out,stderr=err)
 (d/'compile.exit').write_text(str(c.returncode)+'\n');assert c.returncode==0
 binary=None
 for line in (d/'compile.jsonl').read_text().splitlines():
  item=json.loads(line)
  if item.get('reason')=='compiler-artifact' and item.get('target',{}).get('name')=='managed_output' and item.get('executable'):binary=item['executable']
 assert binary;(d/'test-artifact.json').write_text(json.dumps({'path':binary,'sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest()},indent=2)+'\n');settings={'SHARDS_OUTPUT_COST_MEMBERS':'0','SHARDS_OUTPUT_COST_RECORDS':str(d/'observations'),'SHARDS_OUTPUT_COST_DEADLINE_MS':'300000'};runenv=env|settings;(d/'environment.json').write_text(json.dumps(settings,indent=2)+'\n');cmd=[binary,'cost::owner_output_resources_at_explicit_member_count','--exact','--ignored','--nocapture','--test-threads=1'];(d/'command.json').write_text(json.dumps(cmd)+'\n')
 with (d/'test.log').open('wb') as out:c=subprocess.run(cmd,env=runenv,stdout=out,stderr=subprocess.STDOUT)
 (d/'test.exit').write_text(str(c.returncode)+'\n');td=json.loads((d/'observations/teardown.json').read_text());fds=td['exit_observation_fds'];exits=td['host_process_exit'];assert td['owner_stop_and_observer_join']=={'Ok':None} and td['protocol_completed']
 if expected=='identity-rejected':assert c.returncode!=0 and not exits['both_valid_exit_events'] and fds['exact_delta_and_return']
 elif expected=='identity-false-accept':
  snap=json.loads((d/'observations/snapshots.json').read_text())['daemon'];assert c.returncode==0 and exits['both_valid_exit_events'] and snap['state']=='IdentityUnverifiedBeforeRead' and snap['observed_identity'][1]!=snap['expected_start']
 elif expected=='fd-rejected':assert c.returncode!=0 and exits['both_valid_exit_events'] and not fds['exact_delta_and_return'] and fds['after_release']['count']==fds['baseline']['count']+1
 elif expected=='fd-false-accept':assert c.returncode==0 and exits['both_valid_exit_events'] and not fds['exact_delta_and_return'] and fds['after_release']['count']==fds['baseline']['count']+1
 elif expected=='deadline-rejected':assert c.returncode!=0 and fds['exact_delta_and_return'] and not exits['both_valid_exit_events'] and exits['daemon']['state']=='Deadline' and exits['vm']['state']=='Deadline'
 else:assert c.returncode==0 and exits['both_valid_exit_events'] and fds['exact_delta_and_return']
 result={'name':name,'actual_exit':c.returncode,'expected_witness':expected,'teardown':td};results.append(result);(r/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(name,'actual exit',c.returncode,'actual identity/descriptor/deadline witness checked; ownerstop complete',flush=True)
try:
 for name,source,expected in cases:run(name,source,expected)
finally:
 p.write_bytes(original);p.touch();assert p.read_bytes()==original;(r/'restored.sha256').write_text(hashlib.sha256(p.read_bytes()).hexdigest()+'\n');(r/'restored.diff').write_bytes(subprocess.check_output(['git','diff','--',str(p)]))
run('restored',original,'restored')
