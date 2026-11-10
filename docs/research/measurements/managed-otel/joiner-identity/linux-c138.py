from pathlib import Path
import hashlib,json,os,shutil,signal,subprocess,sys,time
SOURCE="c13863223d3b57025b3a2a42d0f882078fb5d1c0"
h=Path.home();repo=h/"managed-capture-check";out=h/"root-managed-otel-native-c138-v1"
out.mkdir(exist_ok=False)
for d in ["empty-docker","tmp","frozen"]:(out/d).mkdir()
env=os.environ.copy()
for k in ["SHARDS_HELPERS","SHARDS_HELPERS_DIR","SHARDS_INIT_BINARY","SHARDS_VM_BINARY","SHARDS_TEST_GUESTS","SHARDS_SERVER_BINARY","SHARDS_TEST_KERNEL","SHARDS_KERNEL","SHARDS_INIT"]:
 env.pop(k,None)
env.update(DOCKER_CONFIG=str(out/"empty-docker"),CARGO_BUILD_JOBS="4",CARGO_TARGET_DIR=str(repo/"target"))
def digest(p):
 with p.open("rb") as f:return hashlib.file_digest(f,"sha256").hexdigest()
def run(name,args,seconds=1800,used_env=None):
 (out/(name+".command.json")).write_text(json.dumps(args)+"\n")
 began=time.monotonic();signals=[];timed=False
 with (out/(name+".log")).open("wb") as log:
  p=subprocess.Popen(args,cwd=repo,env=used_env or env,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
  try:rc=p.wait(timeout=seconds)
  except subprocess.TimeoutExpired:
   timed=True;os.killpg(p.pid,signal.SIGKILL);signals.append(int(signal.SIGKILL))
   try:p.wait(timeout=15)
   except subprocess.TimeoutExpired:pass
   rc=124
 result={"exit_code":rc,"timeout":timed,"signals":signals,"elapsed_seconds":time.monotonic()-began}
 (out/(name+".result.json")).write_text(json.dumps(result,indent=2)+"\n")
 print(name,json.dumps(result),flush=True)
 if rc:print((out/(name+".log")).read_text(errors="replace")[-8000:],flush=True)
 return rc
assert not subprocess.check_output(["git","status","--porcelain","--untracked-files=no"],cwd=repo)
assert run("fetch",["git","fetch","origin","p/managed-layer-production-v1"],120)==0
assert run("checkout",["git","checkout","--detach",SOURCE],120)==0
rules=(repo/"docs/development-rules.md").read_bytes()
assert hashlib.sha256(rules).hexdigest()=="df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3"
assert os.access("/dev/kvm",os.R_OK|os.W_OK)
(out/"identity.json").write_text(json.dumps({"role":"coordinator","source":SOURCE,"policy_sha256":hashlib.sha256(rules).hexdigest(),"uname":list(os.uname()),"disk_free":shutil.disk_usage(repo).free,"admission":"4CPU/8GiB/16GiB; not a hard cgroup/peak assertion","repo":str(repo)},indent=2)+"\n")
assert run("policy",["python3","scripts/check-development-policy"],60)==0
assert run("init-metrics-unit",["cargo","test","--locked","-p","shards-init","--profile","test-release","-j4","--","metrics::","--nocapture","--test-threads=1"],900)==0
assert run("native-clippy",["env","SHARDS_HELPERS=skip","cargo","clippy","--locked","-p","shards-init","-p","shards","--all-targets","-j4","--","-D","warnings"],1200)==0
assert run("normal-carried-build",["cargo","test","--locked","-p","shards","--profile","test-release","--test","managed_output","--test","build","--no-run","--message-format=json","-j4"],2400)==0
artifacts={}
for line in (out/"normal-carried-build.log").read_text().splitlines():
 try:item=json.loads(line)
 except json.JSONDecodeError:continue
 if item.get("reason")=="compiler-artifact" and item.get("executable"):
  name=item.get("target",{}).get("name")
  if name in ["managed_output","build","shards"]:artifacts[name]=item
assert "managed_output" in artifacts and "build" in artifacts,artifacts.keys()
(out/"compiler-artifacts.json").write_text(json.dumps(artifacts,indent=2)+"\n")
guest_env=env.copy();guest_env.update(CC_x86_64_unknown_linux_musl=str(repo/"scripts/zig-cc"),AR_x86_64_unknown_linux_musl=str(repo/"scripts/zig-ar"),ZIG_TARGET="x86_64-linux-musl")
assert run("static-guest",["cargo","build","--locked","-p","shards-testguest","--profile","guest-test","--target","x86_64-unknown-linux-musl","-j4"],900,guest_env)==0
guests=out/"guests/guest";guests.mkdir(parents=True)
guest=repo/"target/x86_64-unknown-linux-musl/guest-test/shards-testguest";shutil.copy2(guest,guests/"shards-testguest")
kernel=repo/"target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400"
assert digest(kernel)=="48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7"
frozen=[]
for name,item in artifacts.items():
 p=Path(item["executable"]);target=out/"frozen"/p.name;shutil.copy2(p,target)
 frozen.append({"name":name,"path":str(target),"bytes":target.stat().st_size,"sha256":digest(target)})
frozen.extend({"name":n,"path":str(p),"bytes":p.stat().st_size,"sha256":digest(p)} for n,p in [("static-guest",guest),("kernel",kernel)])
(out/"artifacts.json").write_text(json.dumps(frozen,indent=2)+"\n")
native=env.copy();native.update(SHARDS_REQUIRE_VMS="1",SHARDS_TEST_GUESTS=str(out/"guests"),SHARDS_TEST_KERNEL=str(kernel),TMPDIR=str(out/"tmp"),XDG_CACHE_HOME=str(out/"runtime-cache"),SHARDS_CACHE=str(out/"runtime-cache/shards"))
for stem,name,test,key in [
 ("actual-otel-kvm","managed_output","metrics::cgroup_metrics_use_declared_scopes_without_merging_member_output","SHARDS_OTEL_RECORDS"),
 ("actual-joiner-kvm","managed_output","metrics::a_joiners_metrics_distinguish_its_run_from_the_shared_microvm","SHARDS_OTEL_JOINER_RECORDS"),
 ("actual-legacy-kvm","build","code_mode_runs_a_program_with_the_callers_grants","SHARDS_CODE_MODE_CAPTURE_RECORDS")]:
 case=native.copy();case[key]=str(out/(stem+"-records"))
 binary=Path(artifacts[name]["executable"])
 rc=run(stem,[str(binary),test,"--exact","--nocapture","--test-threads=1"],300,case)
 text=(out/(stem+".log")).read_text(errors="replace")
 assert rc==0 and "1 passed; 0 failed; 0 ignored;" in text and "SKIP:" not in text,(stem,rc)
 print(text[-5000:],flush=True)
assert not subprocess.check_output(["git","status","--porcelain","--untracked-files=no"],cwd=repo)
(out/"completed.json").write_text(json.dumps({"source":SOURCE,"status":"component and three normal real-VM cases passed; collector/profiles/matrix separate","time_unix_ns":time.time_ns()},indent=2)+"\n")
print("SOURCE",SOURCE,"COMPLETE",str(out),flush=True)

