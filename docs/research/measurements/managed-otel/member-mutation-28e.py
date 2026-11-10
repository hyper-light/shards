from pathlib import Path
import hashlib,json,os,shutil,signal,subprocess,time
SOURCE="28e147c4dda846ca023020e03bc9b00eb2e58e94"
h=Path.home();repo=h/"managed-capture-check";out=h/"root-managed-otel-member-mutation-28e-v1"
out.mkdir(exist_ok=False)
for d in ["empty-docker","tmp"]:(out/d).mkdir()
env=os.environ.copy()
for k in ["SHARDS_HELPERS","SHARDS_HELPERS_DIR","SHARDS_INIT_BINARY","SHARDS_VM_BINARY","SHARDS_TEST_GUESTS","SHARDS_SERVER_BINARY","SHARDS_TEST_KERNEL","SHARDS_KERNEL","SHARDS_INIT"]:env.pop(k,None)
env.update(DOCKER_CONFIG=str(out/"empty-docker"),CARGO_BUILD_JOBS="4",CARGO_TARGET_DIR=str(repo/"target"))
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
assert subprocess.check_output(["git","rev-parse","HEAD"],cwd=repo,text=True).strip()==SOURCE
assert not subprocess.check_output(["git","status","--porcelain","--untracked-files=no"],cwd=repo)
file=repo/"crates/init/src/run.rs";original=file.read_bytes()
needle=b"Scope::Member(domain.member)";replacement=b"Scope::Member(domain.member.saturating_sub(1))"
assert original.count(needle)==1
(out/"before-run.rs").write_bytes(original)
(out/"identity.json").write_text(json.dumps({"role":"coordinator","source":SOURCE,"rules_sha256":sha(repo/"docs/development-rules.md"),"original_run_sha256":hashlib.sha256(original).hexdigest(),"only_mutation":"Stable declared member identity replaced with runnable ordinal before a files-only hole; member-1","expected_forbidden_result":"Actual OTLP member IDs 0 and1 substitute for declared1 and2; exact scope assertion must fail","admission":"4CPU/8GiB/16GiB; not a hard cgroup/peak statement"},indent=2)+"\n")
kernel=repo/"target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400"
assert sha(kernel)=="48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7"
def run(name,args,seconds,used_env=None):
 (out/(name+".command.json")).write_text(json.dumps(args)+"\n")
 started=time.monotonic()
 with (out/(name+".log")).open("wb") as log:
  p=subprocess.Popen(args,cwd=repo,env=used_env or env,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
  try:rc=p.wait(timeout=seconds);timed=False
  except subprocess.TimeoutExpired:
   timed=True;os.killpg(p.pid,signal.SIGKILL);p.wait(timeout=15);rc=124
 result={"exit_code":rc,"timeout":timed,"elapsed_seconds":time.monotonic()-started}
 (out/(name+".result.json")).write_text(json.dumps(result,indent=2)+"\n")
 print(name,json.dumps(result),flush=True)
 return rc
def build_case(label,want):
 assert run(label+"-normal-build",["cargo","test","--locked","-p","shards","--profile","test-release","--test","managed_output","--no-run","--message-format=json","-j4"],1800)==0
 binaries=[]
 for line in (out/(label+"-normal-build.log")).read_text().splitlines():
  try:item=json.loads(line)
  except json.JSONDecodeError:continue
  if item.get("reason")=="compiler-artifact" and item.get("target",{}).get("name")=="managed_output" and item.get("executable"):binaries.append(Path(item["executable"]))
 assert len(binaries)==1
 case=env.copy();case.update(SHARDS_REQUIRE_VMS="1",SHARDS_TEST_GUESTS=str(h/"root-managed-otel-native-28e-v1/guests"),SHARDS_TEST_KERNEL=str(kernel),TMPDIR=str(out/"tmp"),XDG_CACHE_HOME=str(out/(label+"-runtime-cache")),SHARDS_CACHE=str(out/(label+"-runtime-cache/shards")),SHARDS_OTEL_RECORDS=str(out/(label+"-records")))
 binary=binaries[0];shutil.copy2(binary,out/(label+"-test"));(out/(label+"-artifacts.json")).write_text(json.dumps({"test":str(binary),"sha256":sha(binary),"run_rs_sha256":sha(file)},indent=2)+"\n")
 rc=run(label+"-actual-kvm",[str(binary),"metrics::cgroup_metrics_use_declared_scopes_without_merging_member_output","--exact","--nocapture","--test-threads=1"],300,case)
 text=(out/(label+"-actual-kvm.log")).read_text(errors="replace")
 assert "SKIP:" not in text and "running 1 test" in text
 document=json.loads((out/(label+"-records/first.json")).read_bytes())
 members=[]
 for r in document["resourceMetrics"]:
  a={v["key"]:v["value"] for v in r["resource"]["attributes"]}
  if a["shards.cgroup.scope"]["stringValue"]=="member":members.append(int(a["shards.member.id"]["intValue"]))
 if want:
  assert rc==0 and "1 passed; 0 failed; 0 ignored;" in text and sorted(members)==[1,2],(rc,members,text[-3000:])
 else:
  assert rc==101 and "assertion" in text and sorted(members)==[0,1],(rc,members,text[-3000:])
 print(label,"actual exported member IDs",members,flush=True)
 return {"exit":rc,"members":members,"test_sha256":sha(binary),"first_json_sha256":sha(out/(label+"-records/first.json"))}
red=None;green=None
try:
 file.write_bytes(original.replace(needle,replacement));(out/"mutation.diff").write_bytes(subprocess.check_output(["git","diff","--","crates/init/src/run.rs"],cwd=repo))
 red=build_case("mutated",False)
finally:
 file.write_bytes(original)
 assert file.read_bytes()==original
 (out/"restored.sha256").write_text(sha(file)+"\n")
green=build_case("restored",True)
assert not subprocess.check_output(["git","status","--porcelain","--untracked-files=no"],cwd=repo)
result={"source":SOURCE,"mutated":red,"restored":green,"time_unix_ns":time.time_ns()}
(out/"completed.json").write_text(json.dumps(result,indent=2)+"\n");print(json.dumps(result),flush=True)

