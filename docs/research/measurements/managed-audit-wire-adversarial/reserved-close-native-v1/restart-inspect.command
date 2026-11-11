python3 - <<'PY'
from pathlib import Path
import json,os,time
r=Path.home()/'managed-audit-wire-reserved-close-v1'
result={'epoch':time.time(),'root':str(r),'root_exists':r.exists(),'files':{},'new_actor_command_issued_by_controller':False}
for name in ['budget.json','build-records/ready.json','build-records/build.result.json','build-records/clippy.result.json','case-controller-v1/dispatch-before-case.json','case-controller-v1/actor-start.json','case-controller-v1/case.result.json']:
 p=r/name;result['files'][name]={'exists':p.exists(),'json':json.loads(p.read_text()) if p.exists() else None}
for name in ['native-tmp-v1','native-records-v1','case-controller-v1']:
 p=r/name;result[name]={'exists':p.exists(),'entries':sorted(x.name for x in p.iterdir()) if p.is_dir() else None}
s=os.statvfs(r.parent);result['free_bytes']=s.f_bavail*s.f_frsize
print(json.dumps(result),flush=True)
PY