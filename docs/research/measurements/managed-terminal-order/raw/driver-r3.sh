python3 - <<'PY'
import base64,hashlib,json,os,pathlib,shutil,subprocess,sys
base=pathlib.Path('/home/adalundhe/native-terminal-order-light-b02e-v1')
raw=base/'raw-v2';raw.mkdir()
shutil.copyfile(base/'light.py',base/'light-v1.py')
(base/'light.py').write_bytes(base64.b64decode(os.environ.pop('LIGHT_V2_B64')))
env={k:v for k,v in os.environ.items() if not k.startswith('DYLD_')}
env.update(CARGO_HOME=str(base/'cargo-home'),CARGO_TARGET_DIR=str(base/'target'),CARGO_BUILD_JOBS='1',CARGO_INCREMENTAL='0',CARGO_PROFILE_DEV_DEBUG='0',CARGO_PROFILE_TEST_DEBUG='0')
commands=[[sys.executable,str(base/'light.py'),'--source',str(base/'source'),'--output',str(base/'fixture-v2')],['cargo','check','--tests','-j1'],['cargo','clippy','--tests','--no-deps','-j1','--','-D','warnings'],['du','-sk',str(base)]]
receipts=[]
for i,cmd in enumerate(commands):
    if i==1: shutil.copyfile(base/'source/Cargo.lock',base/'fixture-v2/Cargo.lock')
    cwd=base/'fixture-v2' if i in (1,2) else base
    r=subprocess.run(cmd,cwd=cwd,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,timeout=600)
    (raw/f'{i}.log').write_bytes(r.stdout)
    receipts.append({'argv':cmd,'cwd':str(cwd),'exit_code':r.returncode,'log':f'{i}.log','sha256':hashlib.sha256(r.stdout).hexdigest()})
    (raw/'commands.json').write_text(json.dumps(receipts,indent=2)+'\n')
    print(json.dumps(receipts[-1]),flush=True)
    print(r.stdout.decode(errors='replace'),flush=True)
    if r.returncode:sys.exit(r.returncode)
print('COMPILE_ONLY_COMPLETE native_cases_executed=0')
PY