from pathlib import Path
import argparse, difflib, hashlib, json, os, shutil, subprocess

args = argparse.ArgumentParser()
args.add_argument('records', type=Path)
args.add_argument('frozen', type=Path)
a = args.parse_args()
r, frozen = a.records, a.frozen
r.mkdir()
frozen.mkdir()
assert subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip() == '7ee6c4bcf90796c6a1540ed748a5ef89940825e1'
assert subprocess.check_output(['git', 'status', '--porcelain']) == b''
source = Path('crates/testguest/src/managed_network/intercept')
paths = [source/'sock_addr.rs', source/'udp/forward.rs']
originals = {p: p.read_bytes() for p in paths}
for p, raw in originals.items():
    (r/(p.name+'.original')).write_bytes(raw)
binary = json.loads((Path.home()/'root-managed-forward-7ee6-v1/test-artifact.json').read_text())['path']
base = json.loads((Path.home()/'root-managed-forward-7ee6-v1/connected-fixed-original-peer/environment.json').read_text())
env = os.environ.copy() | base
env['CARGO_BUILD_JOBS'] = '4'
env['CARGO_TARGET_DIR'] = str(Path.home()/'managed-capture-check/target')
env['SHARDS_REQUIRE_VMS'] = '1'
env.pop('SHARDS_HELPERS', None)
cmd = [binary, 'direct_guest_two_leg_udp4_records_buffers_and_backend_source_change', '--exact', '--ignored', '--nocapture', '--test-threads=1']
(r/'command.json').write_text(json.dumps(cmd)+'\n')
(r/'source.json').write_text(json.dumps({'head':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'files':{str(p):hashlib.sha256(b).hexdigest() for p,b in originals.items()},'test_artifact':{'path':binary,'sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest()},'scope':'finite private coordinator guest only; no production authority or total kernel budget'},indent=2)+'\n')
results = []

def run(label, mode, witness, budget=18):
    d = r/label
    d.mkdir()
    settings = base | {'SHARDS_SOCK_ADDR_MODE':mode,'SHARDS_SOCK_ADDR_BUFFER_BYTES':str(budget),'SHARDS_NETWORK_RECORDS':str(d)}
    (d/'environment.json').write_text(json.dumps(settings,indent=2)+'\n')
    with (d/'test.log').open('wb') as out:
        child = subprocess.run(cmd, env=env|settings, stdout=out, stderr=subprocess.STDOUT)
    (d/'test.exit').write_text(str(child.returncode)+'\n')
    prefix = f'sock-addr-relay4-{mode}-fixed-original-peer'
    records = []
    for line in (d/(prefix+'.stdout')).read_text(errors='replace').splitlines():
        if line.startswith(('managed-sock-addr ','managed-intercept ')):
            records.append(json.loads(line.split(' ',1)[1]))
    (d/'observed.json').write_text(json.dumps(records,indent=2)+'\n')
    identity = json.loads((d/(prefix+'.identity.json')).read_text())
    guest = Path(identity['guest']['path'])
    guest_digest = identity['guest']['sha256'].removeprefix('sha256:')
    assert len(guest_digest)==64 and all(c in '0123456789abcdef' for c in guest_digest)
    assert hashlib.sha256(guest.read_bytes()).hexdigest() == guest_digest
    saved = frozen/(guest_digest+'.guest')
    if not saved.exists(): shutil.copy2(guest,saved)
    cleanup = [v for v in records if v['type']=='Cleanup' and v['state']=='Observed']
    target = next(v for v in records if v['type']=='Target')
    arrivals = [v for v in records if v['type']=='UdpArrival']
    if witness == 'below-capacity':
        refusal = next(v for v in records if v['type']=='PayloadRefusal')
        assert child.returncode != 0 and refusal['cause']=='Limit' and refusal['provided']==17 and refusal['required']==18
        assert not any(v['type'] in ['Child','LinkAttached','ClientIdentity','ForwardExchange'] for v in records)
        actual = {'refusal':refusal,'cleanup_observation':cleanup,'limit':'No child-release record; early scoped Drop success is NOT independently observed'}
    else:
        assert len(cleanup)==1 and all(cleanup[0][k] for k in ['backend_joined','relay_joined','link_detached','cgroup_removed'])
        reaped = [v for v in records if v['type']=='ChildReaped']
        assert len(reaped)==2 and all(v['reaped'] for v in reaped)
        if witness == 'restored':
            assert child.returncode == 0
            actual = 'strict real two-leg bytes, client presentation, backend source and owned cleanup passed'
        elif witness == 'direct-backend':
            assert child.returncode != 0
            actor = next(v for v in records if v['type']=='ClientIdentity' and v['role']=='ForwardedBackend')
            flow = next(v for v in records if v['type']=='ForwardTarget')
            real = [v for v in arrivals if v['role']=='Backend' and v['ordinal']>2]
            client = next(v for v in records if v['type']=='ClientCompletion' and v['role']=='ForwardedBackend')
            assert len(real)==2 and all(v['peer']==actor['local'] and v['peer']!=flow['upstream'] for v in real)
            assert client['compatibility']=='Exact' and client['success']
            actual = {'backend_received_directly':real,'client_correct_despite_bypass':client}
        elif witness == 'request-corruption':
            assert child.returncode != 0
            actual = [v for v in arrivals if v['role']=='Backend' and v['ordinal']>2 and v['copied_hex']=='5345515545535400ff']
            assert actual, 'actual changed backend bytes required'
        elif witness == 'reply-corruption':
            assert child.returncode != 0
            actual = [v for v in arrivals if v['role']=='SockAddrForwardedClient' and v['copied_hex']=='4341434b454e4400ff']
            assert actual, 'actual changed client bytes required'
        elif witness == 'capacity-guard-removed':
            assert child.returncode != 0
            slots = next(v for v in records if v['type']=='PayloadSlots')
            assert slots['provided']==17 and slots['request_capacity']+slots['reply_capacity']==18
            completion = next(v for v in records if v['type']=='ForwardCompletion')
            assert completion['terminal']=='ScriptComplete' and all(v['success'] for v in records if v['type']=='ClientCompletion')
            actual = {'real_retained_capacity_exceeds_envelope':slots,'guest_forwarded_anyway':completion,'host_guard':'strict host assertion still rejects excess capacity'}
        else:
            raise AssertionError(witness)
    result = {'case':label,'mode':mode,'budget':budget,'actual_exit':child.returncode,'witness':actual,'cleanup':cleanup,'guest_sha256':identity['guest']['sha256']}
    results.append(result)
    (r/'results.json').write_text(json.dumps(results,indent=2)+'\n')
    print(label,child.returncode,'actual witness checked',flush=True)

run('below-capacity','connected','below-capacity',17)
hook = originals[paths[0]]
forward = originals[paths[1]]
request_line = b'            let request = input.slots.request.get(..length).ok_or(Fault::Payload)?;'
reply_line = b'            let reply = input.slots.reply.get(..length).ok_or(Fault::Payload)?;'
required = b'        if required > provided {\n            return Err(ReserveError::Limit { provided, required });\n        }\n'
usable = b'        if usable > provided {\n            return Err(ReserveError::Limit {\n                provided,\n                required: usable,\n            });\n        }\n'
cases = [
    ('removed-Connect4','connected',paths[0],b'(Self::FixedOriginalPeer, Mode::Connected) => &[Connect4, Recvmsg4, Getpeername4]',b'(Self::FixedOriginalPeer, Mode::Connected) => &[Recvmsg4, Getpeername4]','direct-backend',18),
    ('removed-Sendmsg4','unconnected',paths[0],b'(Self::FixedOriginalPeer, Mode::Unconnected) => &[Sendmsg4, Recvmsg4]',b'(Self::FixedOriginalPeer, Mode::Unconnected) => &[Recvmsg4]','direct-backend',18),
    ('request-corrupted-after-validation','connected',paths[1],request_line,b'            if step == Exchange::Binary { if let Some(byte) = input.slots.request.first_mut() { *byte ^= 1; } }\n'+request_line,'request-corruption',18),
    ('reply-corrupted-after-validation','unconnected',paths[1],reply_line,b'            if step == Exchange::Binary { if let Some(byte) = input.slots.reply.first_mut() { *byte ^= 1; } }\n'+reply_line,'reply-corruption',18),
]
assert forward.count(required)==1 and forward.count(usable)==1
capacity_mutant = forward.replace(required,b'',1).replace(usable,b'        let _ = (required, usable);\n',1)
cases.append(('removed-payload-admission','connected',paths[1],forward,capacity_mutant,'capacity-guard-removed',17))
for label,mode,path,needle,replacement,witness,budget in cases:
    original = originals[path]
    assert original.count(needle)==1
    mutant = original.replace(needle,replacement,1)
    (r/(label+'.patch')).write_text(''.join(difflib.unified_diff(original.decode().splitlines(True),mutant.decode().splitlines(True),fromfile=str(path),tofile=str(path))))
    try:
        path.write_bytes(mutant)
        path.touch()
        run(label,mode,witness,budget)
    finally:
        path.write_bytes(original)
        path.touch()
        assert path.read_bytes()==original
        (r/'restored-source.json').write_text(json.dumps({str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in paths},indent=2)+'\n')
        (r/'restored.diff').write_bytes(subprocess.check_output(['git','diff','--',*[str(p) for p in paths]]))
    run('restored-'+label,mode,'restored')
assert subprocess.check_output(['git','status','--porcelain'])==b''
(r/'driver.exit').write_text('0\n')
