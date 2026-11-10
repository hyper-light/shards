from pathlib import Path
import os,json,subprocess,hashlib,shutil,signal,time
h=Path.home();repo=h/"managed-capture-check";out=h/"root-managed-otel-profile-cf8-v1"
out.mkdir(exist_ok=False)
source="cf8f8d600414aced7b24ddfea10410f8ddafd103"
assert not subprocess.check_output(["git","status","--porcelain","--untracked-files=no"],cwd=repo)
subprocess.run(["git","fetch","origin","p/managed-layer-production-v1"],cwd=repo,check=True,timeout=120)
subprocess.run(["git","checkout","--detach",source],cwd=repo,check=True,timeout=120)
manifest=repo/"docs/research/measurements/managed-otel/profile/Cargo.toml"
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
assert sha(repo/"crates/init/src/metrics/otlp.rs")=="0bb86b2f172e12b8053cf30333ccbabd1a6963a315836a639c79de5a4458f28c"
assert sha(repo/"docs/research/measurements/managed-otel-rust-reference/src/reference.rs")=="5e16b6cbf87b221b1e061690c23283197e11571b6aa0535b163bf046df07f4ca"
env=os.environ.copy();env.update(CARGO_BUILD_JOBS="2",CARGO_TARGET_DIR=str(out/"target"))
def run(name,command,seconds=300,used_env=None):
 (out/(name+".command.json")).write_text(json.dumps(command)+"\n")
 started=time.monotonic()
 with (out/(name+".log")).open("wb") as log:
  p=subprocess.Popen(command,cwd=repo,env=used_env or env,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
  try:code=p.wait(timeout=seconds);timed=False
  except subprocess.TimeoutExpired:
   timed=True;os.killpg(p.pid,signal.SIGKILL);p.wait(timeout=15);code=124
 result={"exit":code,"timeout":timed,"elapsed_seconds":time.monotonic()-started}
 (out/(name+".result.json")).write_text(json.dumps(result)+"\n")
 print(name,json.dumps(result),flush=True)
 if code:print((out/(name+".log")).read_text(errors="replace")[-5000:],flush=True)
 return code
assert run("lock",["cargo","generate-lockfile","--manifest-path",str(manifest)],120)==0
shutil.copy2(manifest.parent/"Cargo.lock",out/"Cargo.lock")
common=["cargo","build","--locked","--manifest-path",str(manifest),"--release","-j2"]
assert run("timing-build",common,600)==0
timing=out/"uninstrumented";shutil.copy2(out/"target/release/shards-managed-otel-profile",timing)
assert run("allocation-build",common+["--features","alloc-count"],600)==0
count=out/"allocation-count";shutil.copy2(out/"target/release/shards-managed-otel-profile",count)
assert run("strict-clippy",["cargo","clippy","--locked","--manifest-path",str(manifest),"--all-targets","--all-features","-j2","--","-D","warnings"],300)==0
identity={"source":source,"role":"coordinator","policy_sha256":sha(repo/"docs/development-rules.md"),"codec_sha256":sha(repo/"crates/init/src/metrics/otlp.rs"),"reference_sha256":sha(repo/"docs/research/measurements/managed-otel-rust-reference/src/reference.rs"),"timing_binary_sha256":sha(timing),"allocation_binary_sha256":sha(count),"lock_sha256":sha(out/"Cargo.lock"),"uname":list(os.uname()),"rustc":subprocess.check_output(["rustc","-Vv"],text=True),"load_before":os.getloadavg(),"admission":"2CPU/2GiB/4GiB; explicit root finite profiling batch; not hard cgroup or peak measurement"}
(out/"identity.json").write_text(json.dumps(identity,indent=2)+"\n")
modes=["native-stream","official-compact-buffer","official-pretty-buffer"]
# Nine blocks, rotated order: each mode appears equally at each scheduling position.
for block in range(9):
 for mode in modes[block%3:]+modes[:block%3]:
  name="timing-"+str(block)+"-"+mode
  assert run(name,[str(timing),mode,"1000"],30)==0
for mode in modes:
 assert run("allocations-"+mode,[str(count),mode,"100"],30)==0
(out/"completed.json").write_text(json.dumps({"source":source,"timing_blocks":9,"samples_per_block":1000,"allocation_samples":100,"modes":modes,"load_after":os.getloadavg(),"time_unix_ns":time.time_ns()},indent=2)+"\n")
print("COMPLETE",str(out),flush=True)

