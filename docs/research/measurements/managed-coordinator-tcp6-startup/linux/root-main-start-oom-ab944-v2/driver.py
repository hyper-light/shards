from pathlib import Path
import os,subprocess,json,hashlib,shutil,difflib,traceback
r=Path.home()/'root-main-start-oom-ab944-v2';r.mkdir(exist_ok=False);shutil.copyfile(__file__,r/'driver.py')
(r/'identity.txt').write_text(str(Path.cwd())+'\n'+subprocess.check_output(['git','rev-parse','HEAD'],text=True)+subprocess.check_output(['rustc','-Vv'],text=True)+subprocess.check_output(['uname','-a'],text=True))
mod=Path('crates/vmm/src/vm/mod.rs');arch=Path('crates/vmm/src/vm/x86_64.rs');original={p:p.read_bytes() for p in [mod,arch]}
for p,b in original.items():(r/(p.name+'.saved')).write_bytes(b)
old=Path.home()/'root-main-start-oom-ab944-v1';shutil.copyfile(old/'allocation.rs',r/'allocation.rs')
(old/'root-review.txt').write_text('Actual typed command-line error returned through real VM construction under persistent same-thread allocation refusal. Final descriptor count4→5 caused test101; count alone does not establish leak. Source shows process-wide checked KVM handle retained; v2 records actual descriptor identities cold and repeated, does not warm it away or change production cache. Mutation was not run in v1.\n')
module='''
#[cfg(test)]
#[path = "ALLOC_PATH"]
mod root_faults;
#[cfg(test)]
static ROOT_ARM_CMDLINE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(all(test, hv, target_arch="x86_64", target_os="linux"))]
mod root_actual_start_oom {
 use super::*;
 fn descriptors() -> std::collections::BTreeMap<u32,PathBuf> {
  let observer=PathBuf::from(format!("/proc/{}/fd",std::process::id()));
  std::fs::read_dir("/proc/self/fd").unwrap().map(|entry| {
   let entry=entry.unwrap();(entry.file_name().to_str().unwrap().parse::<u32>().unwrap(),std::fs::read_link(entry.path()).unwrap())
  }).filter(|(_,target)| *target!=observer).collect()
 }
 #[test]
 fn typed_start_returns_after_real_construction_under_refusal() {
  let kernel=std::env::var_os("SHARDS_KERNEL").expect("explicit root experimental kernel");
  let mut cfg=Config::new(kernel.into(),None);cfg.memory_mib=256;cfg.console=Console::Discard;
  cfg.cmdline="x".repeat(crate::arch::x86_64::layout::CMDLINE_MAX);
  for phase in ["cold-host","repeated-host"] {
   let before=descriptors();
   ROOT_ARM_CMDLINE.store(true,std::sync::atomic::Ordering::SeqCst);
   let result=start(&cfg);root_faults::reset();
   let not_reached=ROOT_ARM_CMDLINE.swap(false,std::sync::atomic::Ordering::SeqCst);
   let after=descriptors();assert!(!not_reached,"actual command-line bridge was not reached");
   match result {
    Err(StartError::CommandLine(crate::arch::x86_64::boot::BootError::DoesNotFit{what,at,len})) => {
     assert_eq!(what,"command line");assert_eq!(at,crate::arch::x86_64::layout::CMDLINE);
     eprintln!("ACTUAL_TYPED_START_RETURN phase={phase} what={what} at={at} len={len} before={before:?} after={after:?}");
    }, Err(other)=>panic!("wrong actual startup cause: {other}"),Ok(_)=>panic!("overlong command line unexpectedly booted"),
   }
   let added:Vec<_>=after.iter().filter(|(fd,target)|before.get(fd)!=Some(*target)).collect();
   let removed:Vec<_>=before.iter().filter(|(fd,target)|after.get(fd)!=Some(*target)).collect();
   assert!(removed.is_empty(),"original process descriptors changed: {removed:?}");
   if phase=="cold-host" && !before.values().any(|p|p==std::path::Path::new("/dev/kvm")) {
    let [(_,target)]=added.as_slice() else {panic!("cold-host retained objects differ: {added:?}")};
    assert_eq!(target.as_path(),std::path::Path::new("/dev/kvm"),"additional object is not the process-wide KVM handle");
   } else {assert!(added.is_empty(),"unexpected additional objects: {added:?}");}
  }
 }
}
'''.replace('ALLOC_PATH',str(r/'allocation.rs'))
base_mod=original[mod]+module.encode();marker=b'    let cmdline = super::cmdline::with_machine_options(&told, &required, &mut cmdline_bytes)';assert original[arch].count(marker)==1
hook=b'    #[cfg(test)]\n    if super::ROOT_ARM_CMDLINE.swap(false, std::sync::atomic::Ordering::SeqCst) { super::root_faults::arm(); }\n'
base_arch=original[arch].replace(marker,hook+marker)
(r/'instrumentation.patch').write_text(''.join(''.join(difflib.unified_diff(original[p].decode().splitlines(True),v.decode().splitlines(True),fromfile=str(p),tofile=str(p))) for p,v in [(mod,base_mod),(arch,base_arch)]))
results=[];errors=[]
def run(name,filter):
 cmd=['cargo','test','--locked','-j4','--target','x86_64-unknown-linux-gnu','-p','shards-vmm','--lib',filter,'--','--nocapture','--test-threads=1'];(r/(name+'.command.json')).write_text(json.dumps(cmd)+'\n')
 with (r/(name+'.log')).open('w') as out:rc=subprocess.run(cmd,stdout=out,stderr=subprocess.STDOUT).returncode
 (r/(name+'.exit')).write_text(str(rc)+'\n');print(name,rc,flush=True);results.append(dict(case=name,exit=rc));return rc
try:
 mod.write_bytes(base_mod);arch.write_bytes(base_arch);mod.touch();arch.touch()
 assert run('typed-green','vm::root_actual_start_oom::typed_start_returns_after_real_construction_under_refusal')==0
 unsafe=b'.map_err(super::StartError::CommandLine)?;';assert base_arch.count(unsafe)==1
 mutated=base_arch.replace(unsafe,b'.map_err(|error| super::StartError::Legacy(error.to_string()))?;');arch.write_bytes(mutated);arch.touch()
 (r/'reporting-mutation.patch').write_text(''.join(difflib.unified_diff(base_arch.decode().splitlines(True),mutated.decode().splitlines(True),fromfile=str(arch),tofile=str(arch))))
 rc=run('string-reporting-mutation','vm::root_actual_start_oom::typed_start_returns_after_real_construction_under_refusal');log=(r/'string-reporting-mutation.log').read_text(errors='replace')
 assert rc!=0 and 'memory allocation of' in log and 'SIGABRT' in log
 arch.write_bytes(base_arch);arch.touch();assert run('typed-restored','vm::root_actual_start_oom::typed_start_returns_after_real_construction_under_refusal')==0
except BaseException:
 errors.append(traceback.format_exc());(r/'probe-error.txt').write_text(errors[-1]);print(errors[-1],flush=True)
finally:
 for p,b in original.items():p.write_bytes(b);p.touch();assert p.read_bytes()==b
 (r/'restored-source-sha256.json').write_text(json.dumps({str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in original},indent=2)+'\n');(r/'final.diff').write_bytes(subprocess.check_output(['git','diff','--',str(mod),str(arch)]))
if run('ordinary-composer-restored','vm::cmdline::tests::')!=0:errors.append('ordinary restored composer failed')
if run('ordinary-typed-restored','vm::tests::a_command_line_start_error_preserves_the_cause_and_message')!=0:errors.append('ordinary restored typed unit failed')
(r/'results.json').write_text(json.dumps(results,indent=2)+'\n');(r/'driver.exit').write_text('1\n' if errors else '0\n');raise SystemExit(1 if errors else 0)
