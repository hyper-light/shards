"""Parse every PowerShell literal in the queue without executing any queue step."""
import ast
import base64
import hashlib
import json
from pathlib import Path
import runpy

kit=Path('/Users/adalundhe/g121-a4-winarm')
path=kit/'queue-client-coverage.py'
source=path.read_bytes()
compile(source,str(path),'exec')
tree=ast.parse(source)
blocks=[node.value for node in ast.walk(tree) if isinstance(node,ast.Constant)
        and isinstance(node.value,str) and node.value.startswith("$ErrorActionPreference='Stop';")]
assert len(blocks)==7,len(blocks)
lib=runpy.run_path(str(kit/'trial-vbsoff.py'),run_name='fixture_library')
results=[]
for i,block in enumerate(blocks,1):
    assert len(block)<6500,'Keep each encoded SSH command below Windows command-line limits'
    encoded=base64.b64encode(block.encode()).decode()
    body=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
$source=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('SOURCE'))
$tokens=$null;$errors=$null
$null=[Management.Automation.Language.Parser]::ParseInput($source,[ref]$tokens,[ref]$errors)
if($errors.Count){$errors | Format-List;exit 1}
'Native PS5.1 parsed queue block BLOCK; no queue operation executed.'
exit 0
'''.replace('SOURCE',encoded).replace('BLOCK',str(i))
    status=lib['remote'](body,f'client-queue-parse01-{i:02}',600)
    results.append({'block':i,'source_sha256':hashlib.sha256(block.encode()).hexdigest(),'exit':status.returncode})
    print('Queue block',i,'of',len(blocks),'parse exit',status.returncode,flush=True)
    if status.returncode:break
record={'queue_sha256':hashlib.sha256(source).hexdigest(),'blocks':results,
        'all_passed':len(results)==len(blocks) and all(row['exit']==0 for row in results),
        'scope':'syntax only; no native test or VM launched'}
(kit/'client-queue-parse01.json').write_text(json.dumps(record,indent=2)+'\n')
raise SystemExit(0 if record['all_passed'] else 1)
