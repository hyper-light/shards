"""Build the repository-pinned upstream Docker CLI for Windows arm64 on this Mac."""
import datetime
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess

kit = Path('/Users/adalundhe/g121-a4-winarm')
source = kit / 'docker-cli-29.8.1-source'
out = kit / 'native-clients01'
record = json.loads((out / 'clients.json').read_text())
assert subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip() == record['docker_cli']['commit']
assert not subprocess.check_output(['git','status','--porcelain'],cwd=source,text=True).strip()
assert not (out / 'docker.exe').exists(), 'Do not overwrite an existing native client'
environment = dict(os.environ, GOTOOLCHAIN='go1.26.8', GOMAXPROCS='2', GOFLAGS='-p=2')
go = '/Users/adalundhe/.gvm/gos/go1.26.1/bin/go'
version, goroot = subprocess.check_output([go,'env','GOVERSION','GOROOT'],env=environment,text=True).splitlines()
assert version == 'go1.26.8', version
go = str(Path(goroot) / 'bin/go')
tools = kit / 'native-client-build-tools'
tools.mkdir(exist_ok=True)
gnu_sed = Path('/opt/homebrew/opt/gnu-sed/libexec/gnubin')
assert (gnu_sed/'sed').is_file(), 'Upstream Windows manifest generator needs GNU sed'
environment.update(PATH=str(tools)+':'+str(Path(goroot)/'bin')+':'+str(gnu_sed)+':'+environment['PATH'],
                   GOTOOLCHAIN='local',GOOS='darwin',GOARCH='arm64',CGO_ENABLED='0',GOBIN=str(tools))
# The same resource-generator version upstream's Dockerfile pins.
subprocess.run([go,'install','github.com/josephspurrier/goversioninfo/cmd/goversioninfo@v1.5.0'],
               env=environment,cwd=kit,check=True)
epoch = subprocess.check_output(['git','show','-s','--format=%ct','HEAD'],cwd=source,text=True).strip()
environment.update(GOOS='windows',GOARCH='arm64',CGO_ENABLED='0',GOFLAGS='-mod=vendor -p=2 -trimpath',
                   TARGET=str(out/'build'),VERSION='29.8.1',GITCOMMIT=record['docker_cli']['commit'],
                   SOURCE_DATE_EPOCH=epoch)
subprocess.run(['./scripts/with-go-mod.sh','./scripts/build/binary'],cwd=source,env=environment,check=True)
built = out / 'build/docker-windows-arm64.exe'
data = built.read_bytes()
offset = struct.unpack_from('<I',data,0x3c)[0]
assert data[:2]==b'MZ' and data[offset:offset+4]==b'PE\0\0'
assert struct.unpack_from('<H',data,offset+4)[0]==0xaa64
shutil.copyfile(built,out/'docker.exe')
build_info = subprocess.check_output([go,'version','-m',str(built)],text=True,env=environment)
assert 'GOOS=windows' in build_info and 'GOARCH=arm64' in build_info and 'CGO_ENABLED=0' in build_info
(out/'docker-build-info.txt').write_text(build_info)
record['docker_cli'].update(state='cross-build passed; native version and feature tests pending',
    toolchain=version,cgo_enabled=False,source_date_epoch=epoch,bytes=len(data),
    sha256=hashlib.sha256(data).hexdigest(),pe_machine='ARM64/0xaa64',
    build_method='upstream scripts/with-go-mod.sh scripts/build/binary; vendored dependencies; goversioninfo1.5.0; GNU sed for upstream manifest generation',
    built_at=datetime.datetime.now(datetime.timezone.utc).isoformat())
(out/'clients.json').write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps(record['docker_cli']),flush=True)
