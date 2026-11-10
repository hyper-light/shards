from pathlib import Path
import hashlib,json,os,shutil,signal,subprocess,sys,time
SOURCE="0ae89529036e4e30dba639169844f1bcb5ba84da"
h=Path.home();repo=h/"managed-capture-check";out=h/"root-managed-combined-0ae-v1"
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

case=native.copy();case["SHARDS_OTEL_RECORDS"]=str(out/"ordinary-metrics");case["SHARDS_OTEL_JOINER_RECORDS"]=str(out/"ordinary-joiner")
binary=Path(artifacts["managed_output"]["executable"])
assert run("ordinary-managed-output",[str(binary),"--nocapture","--test-threads=1"],1200,case)==0
ordinary=(out/"ordinary-managed-output.log").read_text(errors="replace")
assert "0 failed;" in ordinary and "SKIP:" not in ordinary
print(ordinary[-7000:],flush=True)
def exact(stem,test,extra):
 used=native|extra
 rc=run(stem,[str(binary),test,"--exact","--ignored","--nocapture","--test-threads=1"],270,used)
 text=(out/(stem+".log")).read_text(errors="replace")
 assert rc==0 and "1 passed; 0 failed; 0 ignored;" in text and "SKIP:" not in text,(stem,rc)
 print(text[-2500:],flush=True)
exact("normal-tenant","tenant_exit::stdio::current_tenant_stdio::normal_exit_preserves_current_neighbor_and_main",{
 "SHARDS_TENANT_EXIT_PORT":"32197","SHARDS_TENANT_EXIT_DEADLINE_MS":"240000","SHARDS_TENANT_EXIT_RECORDS":str(out/"normal-tenant-records"),"SHARDS_TENANT_STDIO_CLEANUP_MS":"15000"})
import uuid
challenge=uuid.uuid4().bytes
pair=lambda m:len(m.encode())*2+len(b"\0stdout\n\xff\x1b[2Jout-tail")+len(b"\0stderr\n\x80\xfeerr-tail")
report=sum(pair(m) for m in ("agent","agent-inherited-ready","agent-inherited-after-release"))+2*(1+4+len(challenge))
exact("inherited-tenant","tenant_exit::stdio::current_tenant_stdio::inherited_child_writers_survive_parent_stdio_closure",{
 "SHARDS_TENANT_STDIO_PORT":"32197","SHARDS_TENANT_STDIO_ACTOR_PORT":"32198","SHARDS_TENANT_STDIO_DEADLINE_MS":"240000","SHARDS_TENANT_STDIO_WORK_MS":"240000","SHARDS_TENANT_STDIO_CLEANUP_MS":"15000","SHARDS_TENANT_STDIO_REPORT_BYTES":str(report),"SHARDS_TENANT_STDIO_CHALLENGE_HEX":challenge.hex(),"SHARDS_TENANT_STDIO_TASKS":"5","SHARDS_TENANT_STDIO_SOURCE_REVISION":SOURCE,"SHARDS_TENANT_STDIO_RECORDS":str(out/"inherited-tenant-records")})
for channel in ("stdout","stderr"):
 exact("header-fin-"+channel,"tenant_exit::current_controls::invalid_application_ack_after_record_preserves_credit_neighbor_and_main",{
 "SHARDS_ADVERSARIAL_CASE":"control-header-only","SHARDS_ADVERSARIAL_CHANNEL":channel,"SHARDS_ADVERSARIAL_PORT":"32197","SHARDS_ADVERSARIAL_PHASE_MS":"240000","SHARDS_ADVERSARIAL_WORK_MS":"240000","SHARDS_ADVERSARIAL_CLEANUP_MS":"15000","SHARDS_ADVERSARIAL_OUTER_MS":"255000","SHARDS_ADVERSARIAL_SOURCE_REVISION":SOURCE,"SHARDS_ADVERSARIAL_RECORDS":str(out/("header-fin-"+channel+"-records"))})
exact("native-resubscribe","tenant_exit::current_controls::native_terminal::canonical_aborted_resubscribe_preserves_errno_and_terminal_credit",{
 "SHARDS_ADVERSARIAL_NATIVE_RUN":str(uuid.uuid4()),"SHARDS_ADVERSARIAL_NATIVE_GENERATION":"1","SHARDS_ADVERSARIAL_NATIVE_CHANNEL":"stdout","SHARDS_ADVERSARIAL_NATIVE_ERRNO":"5","SHARDS_ADVERSARIAL_NATIVE_PHASE_MS":"240000","SHARDS_ADVERSARIAL_NATIVE_CLEANUP_MS":"15000","SHARDS_ADVERSARIAL_NATIVE_OUTER_MS":"255000","SHARDS_ADVERSARIAL_NATIVE_SOURCE_REVISION":SOURCE,"SHARDS_ADVERSARIAL_NATIVE_RECORDS":str(out/"native-resubscribe-records")})
case=native.copy();case["SHARDS_CODE_MODE_CAPTURE_RECORDS"]=str(out/"legacy-records")
rc=run("legacy-code-mode",[str(artifacts["build"]["executable"]),"code_mode_runs_a_program_with_the_callers_grants","--exact","--nocapture","--test-threads=1"],300,case)
assert rc==0 and "1 passed; 0 failed; 0 ignored;" in (out/"legacy-code-mode.log").read_text()
assert not subprocess.check_output(["git","status","--porcelain","--untracked-files=no"],cwd=repo)
(out/"completed.json").write_text(json.dumps({"source":SOURCE,"status":"ordinary full managed-output and explicit normal/inherited/header-fin/native-terminal/legacy cases passed; no whole management closure","time_unix_ns":time.time_ns()},indent=2)+"\n")
print("COMPLETE",SOURCE,str(out),flush=True)

