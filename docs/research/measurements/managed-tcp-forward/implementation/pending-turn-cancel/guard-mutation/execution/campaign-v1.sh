python3 - <<'PY'
from pathlib import Path
import difflib,hashlib,json,os,resource,select,shutil,signal,subprocess,time,tomllib

home=Path.home();root=home/'relay-turn-pending-cancel-guard-0342-v1'
if shutil.disk_usage(home).free<4*1024**3:raise RuntimeError('private4GiB headroom unavailable')
root.mkdir(exist_ok=False);record=root/'record';record.mkdir()
os.sched_setaffinity(0,set(sorted(os.sched_getaffinity(0))[:2]))
resource.setrlimit(resource.RLIMIT_AS,(2*1024**3,2*1024**3));resource.setrlimit(resource.RLIMIT_CORE,(0,0))
end=time.monotonic()+600;before=len(os.listdir('/proc/self/fd'));phases=[];cases=[];errors=[];sources=[];artifacts=[]
source=root/'source';check=root/'check';relay=source/'crates/init/src/relay.rs'
revision='8c56358b537227040d3c2ea03135b3c3a044d5f2';original=None;expected=None;guarded=None;mutated=None;env=None
target='x86_64-unknown-linux-gnu';closure_path='docs/research/measurements/managed-tcp-forward/implementation/pending-turn-cancel/guard-mutation/execution/SOURCE.json'
guard=b'                Self::cancellation_event(ready.revents)?;\n'

def digest(path):
    with path.open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()

def note_error(stage,error):
    errors.append({'stage':stage,'type':type(error).__name__,'detail':str(error)})
    print(json.dumps({'error':errors[-1]}),flush=True)

def phase(name,args,cwd,environment=None,allowed=(0,)):
    remaining=end-time.monotonic()
    if remaining<=0:raise TimeoutError('original600s ended')
    log=record/(name+'.log');child=None;pidfd=None;forced=False;ready=False;status=None
    with log.open('xb') as out:
        try:
            child=subprocess.Popen(args,cwd=cwd,env=environment,stdout=out,stderr=subprocess.STDOUT,start_new_session=True)
            pidfd=os.pidfd_open(child.pid,0)
            try:status=child.wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                forced=True;os.killpg(child.pid,signal.SIGKILL);status=child.wait()
            poll=select.poll();poll.register(pidfd,select.POLLIN);ready=bool(poll.poll(0))
        finally:
            if child is not None and child.returncode is None:
                forced=True;os.killpg(child.pid,signal.SIGKILL);status=child.wait()
            if pidfd is not None:os.close(pidfd)
    row={'name':name,'command':args,'status':status,'allowed_statuses':list(allowed),'pidfd_exit_ready':ready,'reaped':child is not None and child.returncode is not None,'forced_owned_cleanup':forced,'log':str(log),'log_sha256':digest(log)}
    phases.append(row);print(json.dumps({k:row[k] for k in ['name','status','pidfd_exit_ready','reaped','forced_owned_cleanup']}),flush=True)
    if status not in allowed or forced or not ready:
        print(log.read_text(errors='replace')[-7000:],flush=True);raise RuntimeError(name+' failed; original log retained')
    return log

def capture(label,relay_hash):
    actual={name:digest(source/name) for name in expected}
    required=dict(expected);required['crates/init/src/relay.rs']=relay_hash
    if actual!=required:raise RuntimeError('exact source closure differs at '+label)
    diff=subprocess.run(['git','diff','--binary','--','crates/init/src/relay.rs'],cwd=source,capture_output=True,check=True).stdout
    if relay_hash==guarded and diff:raise RuntimeError('restored relay diff is not empty')
    place=record/label;place.mkdir(exist_ok=False)
    (place/'relay.rs').write_bytes(relay.read_bytes());(place/'turn_native.rs').write_bytes((source/'crates/init/src/relay/turn_native.rs').read_bytes())
    (place/'guard.diff').write_bytes(diff)
    row={'label':label,'revision':revision,'source_sha256':actual,'guarded_relay':relay_hash==guarded,'relay_diff_bytes':len(diff),'relay_diff_sha256':digest(place/'guard.diff')}
    (place/'source.json').write_text(json.dumps(row,indent=2)+'\n');sources.append(row)
    return row

def build(state,relay_hash):
    capture(state+'-before-build',relay_hash)
    log=phase('build-'+state,['rustup','run','1.98.1','cargo','test','--lib','--no-run','--target',target,'-j','2','--message-format=json'],check,env)
    found=[]
    for line in log.read_text().splitlines():
        try:
            row=json.loads(line)
            if row.get('reason')=='compiler-artifact' and row.get('executable') and row.get('profile',{}).get('test'):found.append(row['executable'])
        except ValueError:pass
    if len(found)!=1:raise RuntimeError('one exact native executable required')
    frozen=root/'artifacts'/state;frozen.parent.mkdir(exist_ok=True);shutil.copy2(Path(found[0]),frozen)
    if digest(frozen)!=digest(Path(found[0])):raise RuntimeError('frozen binary copy differs')
    artifact={'state':state,'path':str(frozen),'bytes':frozen.stat().st_size,'sha256':digest(frozen),'source':capture(state+'-before-actors',relay_hash)}
    artifacts.append(artifact);(record/('artifact-'+state+'.json')).write_text(json.dumps(artifact,indent=2)+'\n')
    return frozen

def classify_mutant(result,rows):
    def one(kind):
        found=[row for row in rows if row.get('type')==kind]
        return found[0] if len(found)==1 else {}
    snapshots=[row for row in rows if row.get('type')=='NativeTurnSnapshot']
    blocked=[row for row in snapshots if row.get('stage')=='BlockedSend'];attempt=[row for row in snapshots if row.get('stage')=='PendingCancel']
    blocked=blocked[0] if len(blocked)==1 else {};attempt=attempt[0] if len(attempt)==1 else {}
    receipts=[row for row in rows if row.get('type')=='NativeTurnBytes' and row.get('role')=='CancelledBackendProbe']
    receipt=receipts[0] if len(receipts)==1 else {};signal=one('NativeTurnCancelSignal');terminal=one('NativeTurnCancelTerminal');cleanup=one('NativeTurnCleanup')
    ordinary=result['checks'];required_ordinary=['actual_prefill_send_would_block','independent_prefill_receipt','pending_from_actual_source_recv','one_real_block_preserves_suffix_cursors','owned_child_reaped','parent_fd_return','binary_unchanged','same_actual_eventfd_signal8_before_attempt','signal_turn_probe_cleanup_order']
    expected_suffix=blocked.get('suffix_hex','');actual=receipt.get('hex','')
    return {
        'strict_native_red_retained':result.get('status')==101 and result.get('passed') is False,
        'original_real_setup_signal_and_order':all(ordinary.get(key,False) for key in required_ordinary),
        'independent_full_original_suffix':expected_suffix=='524551' and actual==expected_suffix and receipt.get('bytes')==len(bytes.fromhex(actual))==blocked.get('pending')==3,
        'actual_independent_eof0_no_error':terminal.get('eof_read')==0 and terminal.get('eof_error_kind')=='None',
        'coherent_actual_payload_counters':blocked.get('written')==0 and terminal.get('written')==receipt.get('bytes') and terminal.get('extra_consumed')==0 and terminal.get('read')==terminal.get('written',0)+terminal.get('pending',0),
        'original_deadline_and_same_eventfd':signal.get('bytes')==8 and signal.get('value')==1 and signal.get('same_cancel_fd') is True and signal.get('original_remaining_ns',0)>0 and cleanup.get('original_remaining_ns',0)>0,
        'actual_cleanup_before_assertion':cleanup.get('terminal')=='CancelBeforeRecovery' and cleanup.get('fd_before')==cleanup.get('fd_after') and cleanup.get('additional_workers')==0 and cleanup.get('result_ok') is False and cleanup.get('failure_kind')=='Check',
        'actual_turn_logged':bool(attempt) and not terminal.get('typed_cancelled',True),
    }

def native(state,kind,binary,relay_hash):
    capture(state+'-'+kind+'-before-actor',relay_hash)
    if end-time.monotonic()<45:raise TimeoutError('insufficient original total for parent45s')
    supplied=record/(state+'-'+kind+'.inputs.json')
    given={'parent_timeout_ms':45000,'env':{'SHARDS_NETWORK_CASE_MS':'30000','SHARDS_TCP_REQUEST_HEX':'5245515545535400ff0d0a0102','SHARDS_TCP_UP_BYTES':'3','SHARDS_TCP_DOWN_BYTES':'5','SHARDS_TURN_PREFILL_BYTES':'1048576','SHARDS_TURN_PREFILL_SEED_HEX':'00ff0d0a01020304','SHARDS_NATIVE_SNDBUF_BYTES':'4096','SHARDS_NATIVE_RCVBUF_BYTES':'4096'}}
    supplied.write_text(json.dumps(given,indent=2)+'\n')
    place=root/'cases'/(state+'-'+kind);driver=source/'docs/research/measurements/managed-tcp-forward/implementation/pending-turn-cancel/native_case.py'
    red=state=='mutant' and kind=='turn-pending-cancel'
    phase('native-'+state+'-'+kind,['python3',str(driver),'--case',kind,'--binary',str(binary),'--inputs',str(supplied),'--record',str(place)],source,env,(1,) if red else (0,))
    result=json.loads((place/'result.json').read_text());rows=json.loads((place/'observed.json').read_text())
    checks=classify_mutant(result,rows) if red else {'all_existing_strict_checks':result['passed'],'native0':result.get('status')==0}
    item={'state':state,'case':kind,'intentional_negative':red,'result':result,'observed':rows,'campaign_checks':checks,'meets_intended_witness':all(checks.values())}
    cases.append(item);(place/'campaign-classification.json').write_text(json.dumps(checks,indent=2)+'\n')
    print(json.dumps({'state':state,'case':kind,'native_status':result.get('status'),'strict_driver_passed':result['passed'],'campaign_checks':checks}),flush=True)
    if not item['meets_intended_witness']:note_error(state+'-'+kind,RuntimeError('actual intended witness missing; no retry/tuning'))
    return item['meets_intended_witness']

try:
    phase('clone',['git','clone','--filter=blob:none','--single-branch','--branch','p/managed-turn-cancel-0342','git@github.com:hyper-light/shards.git',str(source)],root)
    actual_revision=subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip()
    if actual_revision!=revision:raise RuntimeError('exact immutable source checkpoint required')
    packet=json.loads((source/closure_path).read_text());expected={row['path']:row['sha256'] for row in packet['source_closure']};guarded=packet['guarded_relay_sha256'];mutated=packet['private_mutant_relay_sha256']
    original=relay.read_bytes()
    if digest(source/'docs/development-rules.md')!='df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3':raise RuntimeError('current rules mismatch')
    if original.count(guard)!=1 or hashlib.sha256(original.replace(guard,b'',1)).hexdigest()!=mutated:raise RuntimeError('exact single reviewed guard removal required')
    (record/'original-relay.rs').write_bytes(original)
    (record/'environment.json').write_text(json.dumps({'uname':list(os.uname()),'cpu_affinity':sorted(os.sched_getaffinity(0)),'source':revision,'source_tree':subprocess.check_output(['git','rev-parse','HEAD^{tree}'],cwd=source,text=True).strip(),'policy':packet['policy_revision'],'rules':packet['rules_sha256'],'bounds':packet['bounds'],'kernel_population_return':'Unknown','physical_kernel_caps':'Unverified','cancel_raw_revents_instrumented':False,'cancel_readiness_witness':'guarded typed Cancelled is inferred through exact unchanged native poll classification; no separate raw revents log'},indent=2)+'\n')
    capture('initial',guarded)
    phase('policy',['python3','scripts/check-development-policy'],source)
    phase('format',['rustup','run','1.98.1','rustfmt','--check','--edition','2024','--config','skip_children=true',str(source/'crates/init/src/relay.rs'),str(source/'crates/init/src/relay/turn_native.rs')],source)
    check.mkdir();(source/'crates/testguest/src/interface_root.rs').write_text('#![allow(dead_code)]\nmod managed_network;\n')
    (check/'Cargo.toml').write_text('[package]\nname="relay-guard-native-check"\nversion="0.0.0"\nedition="2024"\nrust-version="1.98.1"\n[workspace]\n[lib]\npath="../source/crates/testguest/src/interface_root.rs"\n[dependencies]\nlibc="=0.2.189"\nshards-devcgroup={path="../source/crates/devcgroup"}\n')
    env=os.environ.copy();env.update({'CARGO_HOME':str(root/'cargo-home'),'CARGO_TARGET_DIR':str(root/'target'),'CARGO_BUILD_JOBS':'2','NUM_JOBS':'2','CARGO_INCREMENTAL':'0','CLIPPY_CONF_DIR':str(source)})
    settings=tomllib.loads((source/'Cargo.toml').read_text())['workspace']['lints'];flags=[]
    for group,items in settings.items():
        for name,value in items.items():
            level=value if isinstance(value,str) else value['level'];flags+=['--'+level,('clippy::' if group=='clippy' else '')+name]
    (record/'workspace-lints.json').write_text(json.dumps(settings,indent=2)+'\n')
    phase('check',['rustup','run','1.98.1','cargo','check','--lib','--target',target,'-j','2'],check,env)
    phase('clippy',['rustup','run','1.98.1','cargo','clippy','--lib','--all-targets','--target',target,'-j','2','--',*flags,'-D','warnings'],check,env)
    baseline=build('baseline',guarded)
    if not native('baseline','turn-pending-cancel',baseline,guarded):raise RuntimeError('observer postimage baseline failed; no mutation')
    if not native('baseline','turn-pressure',baseline,guarded):raise RuntimeError('forward baseline failed; no mutation')
    try:
        relay.write_bytes(original.replace(guard,b'',1))
        mutant=build('mutant',mutated)
        native('mutant','turn-pending-cancel',mutant,mutated)
        native('mutant','turn-pressure',mutant,mutated)
    except Exception as error:note_error('mutant',error)
    finally:
        relay.write_bytes(original)
        capture('finally-restored',guarded)
    restored=build('restored',guarded)
    native('restored','turn-pending-cancel',restored,guarded)
    native('restored','turn-pressure',restored,guarded)
except Exception as error:note_error('campaign',error)
finally:
    if original is not None:
        try:
            relay.write_bytes(original)
            capture('final-source-restored',guarded)
            if subprocess.run(['git','diff','--exit-code'],cwd=source,capture_output=True).returncode:raise RuntimeError('full tracked source diff after restoration')
            (record/'final-git-status.txt').write_text(subprocess.check_output(['git','status','--short'],cwd=source,text=True))
        except Exception as error:note_error('final-restore',error)
    result={'root':str(root),'source_revision':revision,'phases':phases,'sources':sources,'artifacts':artifacts,'cases':cases,'errors':errors,'fd_before':before,'fd_after':len(os.listdir('/proc/self/fd')),'kernel_population_return':'Unknown','physical_kernel_caps':'Unverified','actual_vm_run':False,'native_sequence_count':len(cases),'native_positive0':sum(row['result'].get('status')==0 for row in cases),'native_expected_negative101':sum(row['intentional_negative'] and row['result'].get('status')==101 for row in cases),'passed':not errors and len(cases)==6 and len(artifacts)==3 and all(row['meets_intended_witness'] for row in cases)}
    if result['fd_after']!=before:result['passed']=False;result['errors'].append({'stage':'outer-cleanup','type':'FDReturn','detail':'outer descriptors did not return'})
    (record/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps({key:result[key] for key in ['root','native_sequence_count','native_positive0','native_expected_negative101','errors','fd_before','fd_after','passed']}),flush=True)
raise SystemExit(0 if result['passed'] else 1)
PY
