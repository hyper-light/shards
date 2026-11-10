#!/usr/bin/env python3
"""Run only in the admitted owned Linux fixture envelope; never probes readiness."""
from pathlib import Path
from contextlib import ExitStack
import json
import os
import selectors
import subprocess
import time

OUT = Path(os.environ['MANAGED_PROCESS_EVIDENCE'])
OUT.mkdir(parents=True, exist_ok=False)
BINARIES = {name: Path(os.environ['MANAGED_'+name.upper()+'_BINARY']) for name in ['old','new']}
RESOURCES = os.environ.get('MANAGED_PROCESS_RESOURCES') == '1'


def parent():
    return dict(fds=len(os.listdir('/proc/self/fd')), tasks=len(os.listdir('/proc/self/task')),
                smaps_rollup=Path('/proc/self/smaps_rollup').read_text(),
                status=Path('/proc/self/status').read_text())


def run_case(server_version, client_version, channels, mode):
    case = OUT/f'{server_version}-server-{client_version}-client-c{channels}-{mode}'
    case.mkdir(mode=0o700)
    fixture=case/'private-fixture'
    fixture.mkdir(mode=0o700)
    before=parent()
    children={}
    complete=False
    owner_ended=False
    first_record=False
    finished=set()
    with ExitStack() as opened:
        select=opened.enter_context(selectors.DefaultSelector())
        logs={}
        buffered={}
        def start(role, version):
            environment=dict(os.environ, MANAGED_PROCESS_ROOT=str(fixture), MANAGED_PROCESS_ROLE=role,
                             MANAGED_PROCESS_MODE=mode, MANAGED_PROCESS_CHANNELS=str(channels))
            if not RESOURCES: environment.pop('MANAGED_PROCESS_RESOURCES',None)
            error=opened.enter_context((case/(role+'.stderr')).open('wb'))
            logs[role]=opened.enter_context((case/(role+'.stdout')).open('wb'))
            command=[str(BINARIES[version]),'managed_process_role','--ignored','--exact','--test-threads=1','--nocapture']
            (case/(role+'.command.json')).write_text(json.dumps(command))
            child=subprocess.Popen(command,env=environment,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=error)
            children[role]=child
            buffered[role]=b''
            select.register(child.stdout,selectors.EVENT_READ,role)
        try:
            start('server',server_version)
            deadline=time.monotonic()+120
            while select.get_map():
                left=deadline-time.monotonic()
                if left<=0: raise TimeoutError(str(case))
                events=select.select(left)
                if not events: raise TimeoutError(str(case))
                for event,_ in events:
                    role=event.data
                    data=os.read(event.fd,65536)
                    if not data:
                        select.unregister(event.fileobj)
                        event.fileobj.close()
                        code=children[role].wait()
                        (case/(role+'.exit')).write_text(str(code)+'\n')
                        if role=='client' and not owner_ended:
                            children['server'].stdin.close()
                            owner_ended=True
                        if code!=0: raise RuntimeError((str(case),role,code))
                        continue
                    logs[role].write(data)
                    buffered[role]+=data
                    while b'\n' in buffered[role]:
                        line,buffered[role]=buffered[role].split(b'\n',1)
                        text=line.decode(errors='replace')
                        if role=='server' and 'managed_ready' in text:
                            assert 'client' not in children
                            start('client',client_version)
                        if role=='server' and text.startswith('managed_finished\t'):
                            finished.add(int(text.split('\t')[1]))
                        if role=='client' and text.startswith('managed_record\t0'):
                            first_record=True
                        if mode=='stall' and first_record and finished==set(range(1,channels)) and not owner_ended:
                            children['server'].stdin.close()
                            owner_ended=True
            assert 'client' in children
            complete=True
        finally:
            for child in children.values():
                if child.stdin and not child.stdin.closed: child.stdin.close()
            if not complete:
                for child in children.values(): child.kill()
            for child in children.values():
                child.wait()
                if child.stdout and not child.stdout.closed: child.stdout.close()
    after=parent()
    assert (before['fds'],before['tasks'])==(after['fds'],after['tasks']),str(case)
    (case/'parent-before.json').write_text(json.dumps(before))
    (case/'parent-after.json').write_text(json.dumps(after))
    for role in ['server','client']:
        log=(case/(role+'.stdout')).read_text()
        assert 'managed_process_return\t'+role in log,(str(case),role,'no resource return')
    (case/'result.json').write_text(json.dumps(dict(pass_result=True,server=server_version,client=client_version,channels=channels,mode=mode,resources=RESOURCES)))
    print(case.name,'passed',flush=True)


for server,client in [('old','new'),('new','old')]:
    for channels in [2,4]:
        for mode in (['normal','stall'] if RESOURCES else ['normal','stall','wrong_server','wrong_client']):
            run_case(server,client,channels,mode)
(OUT/'complete.json').write_text(json.dumps(dict(cases=8 if RESOURCES else 16,role_processes=16 if RESOURCES else 32)))
