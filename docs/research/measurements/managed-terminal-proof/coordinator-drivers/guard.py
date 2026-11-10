from pathlib import Path
import hashlib,json,os,re,socket,subprocess,uuid
h=Path.home();repo=h/'managed-capture-check';record=h/'root-managed-terminal-guard-b02e-v1';record.mkdir(exist_ok=False);os.chdir(repo)
source='b02e8592c547553c788702ac6c52ea6b633e875d';assert subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()==source
assert not subprocess.check_output(['git','status','--porcelain'],text=True).strip()
p=repo/'crates/shards/src/daemon/managed.rs';original=p.read_bytes();sha=hashlib.sha256(original).hexdigest();assert sha=='2b3e4d2c80be615a58b062b6a077c11511e2679c738c798a27f0e014a880f655'
(record/'original.sha256').write_text(sha+'\n');(record/'source.txt').write_text(source+'\n')
anchor=b'(Self::Draining(Terminal::End), State::Finished(_), _) => Self::Ended,'
assert original.count(anchor)==1
mutant=original.replace(anchor,b'(Self::Draining(Terminal::End), State::Finished(_), _) if false => Self::Ended,')
base=json.loads((h/'root-managed-terminal-after-b02e-v2/environment.json').read_text());base['DOCKER_CONFIG']=str(record/'empty-docker');(record/'empty-docker').mkdir()
env=os.environ.copy()|base
for key in ['SHARDS_HELPERS','SHARDS_INIT_BINARY','SHARDS_SERVER_BINARY','SHARDS_HELPERS_DIR','SHARDS_VM_BINARY','SHARDS_TEST_GUESTS']:env.pop(key,None)
def compile_case(name,tag):
 folder=record/name;folder.mkdir();e=env|{'SHARDS_TENANT_EXIT_SOURCE_REVISION':tag,'SHARDS_TENANT_STDIO_SOURCE_REVISION':tag,'SHARDS_OUTPUT_COST_SOURCE_REVISION':tag}
 args=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_output','--no-run','--message-format=json'];(folder/'command.json').write_text(json.dumps(args)+'\n')
 with (folder/'stdout').open('wb') as out,(folder/'stderr').open('wb') as err:q=subprocess.run(args,env=e,stdout=out,stderr=err,timeout=900)
 (folder/'exit').write_text(str(q.returncode)+'\n');assert q.returncode==0,'compile failed'
 binaries=[v['executable'] for line in (folder/'stdout').read_text().splitlines() if (v:=json.loads(line)).get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable')]
 assert binaries;binary=binaries[-1];(folder/'artifact.json').write_text(json.dumps({'path':binary,'sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest(),'head':source,'source_variant':tag,'daemon_sha256':hashlib.sha256(p.read_bytes()).hexdigest()},indent=2)+'\n');return binary,e
results=[]
def run_case(name,kind,binary,e,expected):
 folder=record/name;folder.mkdir()
 with socket.socket() as a,socket.socket() as b:
  a.bind(('127.0.0.1',0));b.bind(('127.0.0.1',0));port=a.getsockname()[1];actor=b.getsockname()[1]
 assert port!=actor
 if kind=='normal':
  test='tenant_exit::member_normal_exit_preserves_sibling_and_main_progress';settings={'SHARDS_TENANT_EXIT_PORT':str(port),'SHARDS_TENANT_EXIT_DEADLINE_MS':'300000','SHARDS_TENANT_EXIT_RECORDS':str(folder/'records')}
 else:
  test='tenant_exit::stdio::actor_stdio_eof_preserves_actor_control_and_neighbor_lifetime';challenge=uuid.uuid4().bytes;envelope=len(b'agent\x00stdout\n\xff\x1b[2Jout-tail')+len(b'agent\x00stderr\n\x80\xfeerr-tail')+2*(1+4+len(challenge))
  settings={'SHARDS_TENANT_STDIO_PORT':str(port),'SHARDS_TENANT_STDIO_ACTOR_PORT':str(actor),'SHARDS_TENANT_STDIO_DEADLINE_MS':'300000','SHARDS_TENANT_STDIO_WORK_MS':'300000','SHARDS_TENANT_STDIO_CLEANUP_MS':'300000','SHARDS_TENANT_STDIO_REPORT_BYTES':str(envelope),'SHARDS_TENANT_STDIO_CHALLENGE_HEX':challenge.hex(),'SHARDS_TENANT_STDIO_RECORDS':str(folder/'records')}
 args=[binary,test,'--exact','--ignored','--nocapture','--test-threads=1'];(folder/'command.json').write_text(json.dumps(args)+'\n');(folder/'environment.json').write_text(json.dumps(settings,indent=2)+'\n')
 with (folder/'test.log').open('wb') as out:q=subprocess.run(args,env=e|settings,stdout=out,stderr=subprocess.STDOUT,timeout=900)
 (folder/'test.exit').write_text(str(q.returncode)+'\n');t=json.loads((folder/'records/teardown.json').read_text());log=(folder/'test.log').read_text()
 if kind=='normal':cleanup=t['private_run_cleanup_completed'] and t['owner_stop_and_observer_join']['completed']
 else:cleanup=t['run_removed'] and t['owner_stop_observer_join']['complete']
 row={'case':name,'kind':kind,'actual_exit':q.returncode,'expected_exit':expected,'cleanup_observed':cleanup,'teardown':t}
 if expected==101:
  end_tails={}
  for f in (folder/'records').glob('wire-1-*.bin'):
   b=f.read_bytes()
   if len(b)>1 and b[:2]==bytes([1,2]):
    parts=f.stem.split('-');end_tails[int(parts[2])]=int(parts[3])+len(b)
  observations=[]
  for meta in ('managed-output.before-cleanup','managed-output.stopped'):
   f=folder/'records'/meta
   if f.exists():
    for match in re.finditer(r'member 1 channel ([12]) state aborted accepted ([0-9]+) consumed ([0-9]+) tail unknown',f.read_text()):
     ch,accepted,consumed=map(int,match.groups())
     if accepted==consumed==end_tails.get(ch):observations.append({'metadata':meta,'channel':ch,'accepted':accepted,'consumed':consumed,'canonical_end_wire_tail':end_tails[ch]})
  row['actual_canonical_end_tails']=end_tails;row['actual_false_abort_rows']=observations
 results.append(row);(record/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(name,q.returncode,'cleanup',cleanup,flush=True)
 assert q.returncode==expected and cleanup,row
 assert ('1 failed' if expected else '1 passed; 0 failed') in log,log[-1200:]
 if expected==101:assert row['actual_false_abort_rows'],'no corresponding external false-abort witness'
 else:assert (t.get('protocol_complete') if kind=='stdio' else t.get('protocol_completed')),t
 return row
problem=None
try:
 p.write_bytes(mutant);(record/'mutant.sha256').write_text(hashlib.sha256(mutant).hexdigest()+'\n')
 (record/'mutation.patch').write_bytes(subprocess.check_output(['git','diff','--','crates/shards/src/daemon/managed.rs']))
 binary,e=compile_case('mutant-build',source+'+canonical-finish-guard-disabled')
 run_case('mutant-stdio','stdio',binary,e,101)
except Exception as error:
 problem=repr(error);(record/'problem.json').write_text(json.dumps({'error':problem,'postcase_cleanup':'See actual retained percase result; not inferred from source restoration'},indent=2)+'\n')
finally:
 p.write_bytes(original);assert hashlib.sha256(p.read_bytes()).hexdigest()==sha
 diff=subprocess.check_output(['git','diff','--','crates/shards/src/daemon/managed.rs']);(record/'restored.diff').write_bytes(diff);assert not diff
 assert not subprocess.check_output(['git','status','--porcelain'],text=True).strip()
 (record/'restored.sha256').write_text(hashlib.sha256(p.read_bytes()).hexdigest()+'\n');print('exact source restored',flush=True)
if problem:raise RuntimeError(problem)
binary,e=compile_case('restored-build',source)
run_case('restored-normal','normal',binary,e,0)
run_case('restored-stdio','stdio',binary,e,0)
(record/'driver.exit').write_text('0\n');print('guard and restored gate0',flush=True)

