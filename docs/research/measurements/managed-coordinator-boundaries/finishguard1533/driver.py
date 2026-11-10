from pathlib import Path
import os,json,subprocess,hashlib,time,difflib
h=Path.home();root=h/'managed-combined-seven-target-check';os.chdir(root)
r=h/'root-managed-finish-guard-1533-v1';r.mkdir(exist_ok=False);(r/'driver.py').write_text(Path(__file__).read_text());(r/'source.txt').write_text(subprocess.check_output(['git','rev-parse','HEAD'],text=True))
file=Path('crates/remote/src/managed.rs');original=file.read_bytes();(r/'original.sha256').write_text(hashlib.sha256(original).hexdigest()+'\n')
env=os.environ.copy()|{'CARGO_BUILD_JOBS':'4','CARGO_TARGET_DIR':str(h/'.cache/managed-combined-seven-target-b8fe')}
def run(name,filter,expected):
 d=r/name;d.mkdir();cmd=['lockf','/tmp/g121-mac.lock','cargo','test','--locked','-j4','-p','shards-remote','--lib','--target','aarch64-apple-darwin',filter,'--','--test-threads=1'];(d/'command.json').write_text(json.dumps(cmd)+'\n')
 with (d/'test.log').open('wb') as out:p=subprocess.run(cmd,env=env,stdout=out,stderr=subprocess.STDOUT)
 (d/'test.exit').write_text(str(p.returncode)+'\n')
 text=(d/'test.log').read_text();lines=[l for l in text.splitlines() if 'Running unittests ' in l]
 if lines:
  binary=Path(lines[-1].rsplit(' (',1)[1].rstrip(')'));(d/'test-artifact.json').write_text(json.dumps({'path':str(binary),'sha256':hashlib.sha256(binary.read_bytes()).hexdigest()},indent=2)+'\n')
 print(name,p.returncode,flush=True);assert p.returncode==expected,text[-4000:]
 return text
try:
 s=original.decode();a=s.index('        match self.sender.try_send(Command::Handler(())) {',s.index('    pub fn finish(&self)'));b=s.index('\n    }\n\n    /// Revocation',a)
 mutated=s[:a]+'        self.sender.send(Command::Handler(())).map_err(Error::Transport)'+s[b:]
 (r/'mutation.patch').write_text(''.join(difflib.unified_diff(s.splitlines(True),mutated.splitlines(True),fromfile=str(file),tofile=str(file))));file.write_text(mutated);os.utime(file,ns=(time.time_ns(),time.time_ns()))
 text=run('coalescing-removed','managed::tests::finish_',101);assert '1 passed; 2 failed' in text and "admitted command slots are full" in text
finally:
 file.write_bytes(original);os.utime(file,ns=(time.time_ns(),time.time_ns()));assert file.read_bytes()==original;(r/'restored.sha256').write_text(hashlib.sha256(file.read_bytes()).hexdigest()+'\n')
 diff=subprocess.check_output(['git','diff','--',str(file)]);(r/'restored.diff').write_bytes(diff);assert not diff
text=run('restored','managed::tests',0);assert '12 passed; 0 failed' in text;(r/'driver.exit').write_text('0\n')
