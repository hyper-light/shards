from pathlib import Path
import os,subprocess,json,shutil,hashlib,time
h=Path.home();repo=h/"managed-capture-check";out=h/"root-managed-otel-profile-acd-v1";out.mkdir(exist_ok=False)
source="acd0b8daf"
subprocess.run(["git","fetch","origin","p/managed-layer-production-v1"],cwd=repo,check=True,timeout=120)
subprocess.run(["git","checkout","--detach",source],cwd=repo,check=True,timeout=120)
source=subprocess.check_output(["git","rev-parse","HEAD"],cwd=repo,text=True).strip()
manifest=repo/"docs/research/measurements/managed-otel/profile/Cargo.toml"
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
env=os.environ.copy();env.update(CARGO_TARGET_DIR=str(h/"root-managed-otel-profile-cf8-v1/target"),CARGO_BUILD_JOBS="2")
command=["cargo","build","--locked","--manifest-path",str(manifest),"--release","--features","alloc-count","-j2"]
with (out/"build.log").open("wb") as log:p=subprocess.run(command,cwd=repo,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=300)
assert p.returncode==0
binary=out/"allocation-count";shutil.copy2(Path(env["CARGO_TARGET_DIR"])/"release/shards-managed-otel-profile",binary)
rows=[]
for mode in ("native-stream","official-compact-buffer","official-pretty-buffer"):
 command=[str(binary),mode,"100"];(out/(mode+".command.json")).write_text(json.dumps(command)+"\n")
 with (out/(mode+".json")).open("wb") as log:p=subprocess.run(command,stdout=log,stderr=subprocess.PIPE,timeout=30)
 assert p.returncode==0,p.stderr
 result=json.loads((out/(mode+".json")).read_bytes());a=result["allocations"]
 assert a["live_before"]==a["live_after"],a
 rows.append({"mode":mode,"n":result["iterations"],"allocations":a,"bytes_per_request":result["output_bytes"]//result["iterations"]})
oracle=h/"root-managed-otel-oracle-fedora-28e-v1/oracle"
reference=repo/"docs/research/measurements/managed-otel-rust-reference/records/light-v1/payloads"
receipts=[]
for mode in ("native-stream","official-compact-buffer","official-pretty-buffer"):
 path=reference/(mode+".json");command=[str(oracle),"-max-bytes",str(path.stat().st_size),"-input",str(path)]
 with (out/(mode+".receipt.json")).open("wb") as log:p=subprocess.run(command,stdout=log,stderr=subprocess.PIPE,timeout=30)
 assert p.returncode==0,p.stderr
 receipts.append(json.loads((out/(mode+".receipt.json")).read_bytes()))
assert receipts[0]==receipts[1]==receipts[2]
command=["cargo","clippy","--locked","--manifest-path",str(manifest),"--all-targets","--all-features","-j2","--","-D","warnings"]
with (out/"clippy.log").open("wb") as log:p=subprocess.run(command,cwd=repo,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=180)
assert p.returncode==0
result={"source":source,"role":"coordinator","binary_sha256":sha(binary),"allocation_source_sha256":sha(manifest.parent/"src/allocations.rs"),"rows":rows,"official_decoder_receipts_equal":True,"unmodified_timing_source":"cf8f8d600414aced7b24ddfea10410f8ddafd103","unmodified_timing_archive":"root-managed-otel-profile-cf8-v1","time_unix_ns":time.time_ns(),"scope":"only corrected post-measurement retained-byte receipt; actual path System calls; not full SDK or VM"}
(out/"result.json").write_text(json.dumps(result,indent=2)+"\n");print(json.dumps(result),flush=True)

