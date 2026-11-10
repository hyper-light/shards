from pathlib import Path
import subprocess,os,json,signal,time,hashlib,shutil
h=Path.home();repo=h/"managed-capture-check";out=h/"root-managed-otel-joiner-red-9be-v1";out.mkdir(exist_ok=False)
source="9be1921d4";assert not subprocess.check_output(["git","status","--porcelain","--untracked-files=no"],cwd=repo)
subprocess.run(["git","fetch","origin","p/managed-layer-production-v1"],cwd=repo,check=True,timeout=120);subprocess.run(["git","checkout","--detach",source],cwd=repo,check=True,timeout=120)
source=subprocess.check_output(["git","rev-parse","HEAD"],cwd=repo,text=True).strip()
env=os.environ.copy()
for k in ["SHARDS_HELPERS","SHARDS_HELPERS_DIR","SHARDS_INIT_BINARY","SHARDS_VM_BINARY","SHARDS_TEST_GUESTS","SHARDS_SERVER_BINARY","SHARDS_TEST_KERNEL","SHARDS_KERNEL","SHARDS_INIT"]:env.pop(k,None)
(out/"empty-docker").mkdir();(out/"tmp").mkdir();env.update(DOCKER_CONFIG=str(out/"empty-docker"),CARGO_BUILD_JOBS="4",CARGO_TARGET_DIR=str(repo/"target"))
command=["cargo","test","--locked","-p","shards","--profile","test-release","--test","managed_output","--no-run","--message-format=json","-j4"]
(out/"build.command.json").write_text(json.dumps(command)+"\n")
with (out/"build.log").open("wb") as log:p=subprocess.run(command,cwd=repo,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=1800)
assert p.returncode==0
binary=None
for line in (out/"build.log").read_text().splitlines():
 try:a=json.loads(line)
 except json.JSONDecodeError:continue
 if a.get("reason")=="compiler-artifact" and a.get("target",{}).get("name")=="managed_output" and a.get("executable"):binary=Path(a["executable"])
assert binary
shutil.copy2(binary,out/"fixture")
env.update(SHARDS_REQUIRE_VMS="1",SHARDS_TEST_GUESTS=str(h/"root-managed-otel-native-28e-v1/guests"),SHARDS_TEST_KERNEL=str(repo/"target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400"),TMPDIR=str(out/"tmp"),XDG_CACHE_HOME=str(out/"runtime-cache"),SHARDS_CACHE=str(out/"runtime-cache/shards"),SHARDS_OTEL_JOINER_RECORDS=str(out/"records"))
command=[str(binary),"metrics::a_joiners_metrics_distinguish_its_run_from_the_shared_microvm","--exact","--nocapture","--test-threads=1"];(out/"actual.command.json").write_text(json.dumps(command)+"\n")
with (out/"actual.log").open("wb") as log:p=subprocess.run(command,cwd=repo,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=300)
print("actual_exit",p.returncode);print((out/"actual.log").read_text(errors="replace")[-6000:])
result={"source":source,"exit":p.returncode,"fixture_sha256":hashlib.sha256(binary.read_bytes()).hexdigest(),"time_unix_ns":time.time_ns()}
if (out/"records/joiner.json").exists():
 doc=json.loads((out/"records/joiner.json").read_bytes());result["joiner_resource"]=[x["resource"] for x in doc["resourceMetrics"]];result["provider_id"]=(out/"records/provider.id").read_text();result["joiner_id"]=(out/"records/joiner.id").read_text()
(out/"result.json").write_text(json.dumps(result,indent=2)+"\n");print(json.dumps(result))
assert p.returncode==101 and (out/"records/joiner.json").exists()

