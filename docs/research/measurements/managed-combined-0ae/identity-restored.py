"""Preserve the split-stderr wrapper failure; validate its actual red and run green."""
from pathlib import Path
import hashlib,json,os,shutil,signal,subprocess,time
h=Path.home();repo=h/"managed-capture-check";before=h/"root-managed-identity-closing-0ae-v1";out=h/"root-managed-identity-restored-0ae-v1"
out.mkdir(exist_ok=False)
source="0ae89529036e4e30dba639169844f1bcb5ba84da"
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
assert subprocess.check_output(["git","rev-parse","HEAD"],cwd=repo,text=True).strip()==source
assert not subprocess.check_output(["git","status","--porcelain","--untracked-files=no"],cwd=repo)
assert (repo/"crates/shards/src/daemon/metrics.rs").read_bytes()==(before/"before-metrics.rs").read_bytes()
red=before/"mutated-records"
provider=(red/"provider.id").read_text();joiner=(red/"joiner.id").read_text()
document=json.loads((red/"joiner.json").read_bytes());assert len(document["resourceMetrics"])==1
attrs={a["key"]:a["value"] for a in document["resourceMetrics"][0]["resource"]["attributes"]}
assert attrs["shards.microvm.id"]["stringValue"]==attrs["shards.run.id"]["stringValue"]==joiner!=provider
assert json.loads((before/"mutated-kvm.result.json").read_bytes())["exit_code"]==101
assert "metrics.rs:245:9" in (before/"mutated-kvm.stderr").read_text() and "assertion `left == right` failed" in (before/"mutated-kvm.stderr").read_text()
assert "observer_fds before=7 after=7" in (before/"mutated-kvm.log").read_text()
(out/"red-readback.json").write_text(json.dumps({"source":source,"provider":provider,"wrong_exported_microvm":joiner,"exit_code":101,"stdout_sha256":sha(before/"mutated-kvm.log"),"stderr_sha256":sha(before/"mutated-kvm.stderr"),"original_wrapper_exit":1,"wrapper_failure":"looked for assertion in stdout; actual test panic is separate stderr; source finally restored","source_restored_sha256":sha(repo/"crates/shards/src/daemon/metrics.rs")},indent=2)+"\n")
env=os.environ.copy()
for k in ["SHARDS_HELPERS","SHARDS_HELPERS_DIR","SHARDS_INIT_BINARY","SHARDS_VM_BINARY","SHARDS_TEST_GUESTS","SHARDS_SERVER_BINARY","SHARDS_TEST_KERNEL","SHARDS_KERNEL","SHARDS_INIT"]:env.pop(k,None)
for n in ["tmp","empty-docker"]:(out/n).mkdir()
env.update(CARGO_BUILD_JOBS="4",CARGO_TARGET_DIR=str(repo/"target"),DOCKER_CONFIG=str(out/"empty-docker"))
def run(name,args,seconds,used):
 (out/(name+".command.json")).write_text(json.dumps(args)+"\n");start=time.monotonic()
 with (out/(name+".log")).open("wb") as log:
  p=subprocess.Popen(args,cwd=repo,env=used,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
  try:rc=p.wait(timeout=seconds);timed=False
  except subprocess.TimeoutExpired:os.killpg(p.pid,signal.SIGKILL);p.wait(timeout=15);rc=124;timed=True
 result={"exit_code":rc,"timeout":timed,"elapsed_seconds":time.monotonic()-start};(out/(name+".result.json")).write_text(json.dumps(result,indent=2)+"\n");print(name,json.dumps(result),flush=True);return rc
assert run("restored-normal-build",["cargo","test","--locked","-p","shards","--profile","test-release","--test","managed_output","--no-run","--message-format=json","-j4"],1800,env)==0
artifacts=[]
for line in (out/"restored-normal-build.log").read_text().splitlines():
 try:item=json.loads(line)
 except json.JSONDecodeError:continue
 if item.get("reason")=="compiler-artifact" and item.get("target",{}).get("name")=="managed_output" and item.get("executable"):artifacts.append(Path(item["executable"]))
assert len(artifacts)==1
kernel=repo/"target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400";assert sha(kernel)=="48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7"
used=env|{"SHARDS_REQUIRE_VMS":"1","SHARDS_TEST_GUESTS":str(h/"root-managed-combined-0ae-v2/guests"),"SHARDS_TEST_KERNEL":str(kernel),"TMPDIR":str(out/"tmp"),"XDG_CACHE_HOME":str(out/"runtime-cache"),"SHARDS_CACHE":str(out/"runtime-cache/shards"),"SHARDS_OTEL_JOINER_RECORDS":str(out/"restored-records")}
shutil.copy2(artifacts[0],out/"restored-test")
assert run("restored-kvm",[str(artifacts[0]),"metrics::a_joiners_metrics_distinguish_its_run_from_the_shared_microvm","--exact","--nocapture","--test-threads=1"],300,used)==0
text=(out/"restored-kvm.log").read_text();assert "1 passed; 0 failed; 0 ignored;" in text and "SKIP:" not in text and "observer_fds before=7 after=7" in text
records=out/"restored-records";provider=(records/"provider.id").read_text();joiner=(records/"joiner.id").read_text();document=json.loads((records/"joiner.json").read_bytes());assert len(document["resourceMetrics"])==1
attrs={a["key"]:a["value"] for a in document["resourceMetrics"][0]["resource"]["attributes"]};assert attrs["shards.microvm.id"]["stringValue"]==provider!=joiner and attrs["shards.run.id"]["stringValue"]==joiner
assert not subprocess.check_output(["git","status","--porcelain","--untracked-files=no"],cwd=repo)
(out/"completed.json").write_text(json.dumps({"source":source,"actual_restored_microvm":provider,"joiner":joiner,"test_sha256":sha(out/"restored-test"),"export_sha256":sha(records/"joiner.json"),"source_sha256":sha(repo/"crates/shards/src/daemon/metrics.rs"),"restored_test_exit":0,"raw_red_validated":True,"original_failed_wrapper_preserved":True,"time_unix_ns":time.time_ns()},indent=2)+"\n");print("COMPLETE",source,str(out),flush=True)
