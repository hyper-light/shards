#!/usr/bin/env python3
"""Compare actual OPA/Shards calls; preserve observed upstream defects separately."""
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
def records(path, prefix):
    out=[]
    for line in path.read_text().splitlines():
        if prefix in line:
            out.append(json.loads(line.split(prefix,1)[1]))
    return out
opa=records(root/'opa.log','REGOCACHE_CASE ')
shards=records(root/'shards.log','REGOCACHE_CASE ')
assert len(opa)==len(shards)==4, (len(opa),len(shards))
fixtures=json.loads((pathlib.Path(__file__).parent/'cases.json').read_text())
assert [c['name'] for c in fixtures]==[c['name'] for c in opa]
key=lambda instruction: (instruction['name'],tuple(instruction['args']))
# Each distinct operation appears first in one fresh-cache scenario. Derive
# independent-call controls from those actual observed cold calls, not a guess.
cold={key(c['calls'][0]):r['answers'][0] for c,r in zip(fixtures,opa)}
report=[]
for case,a,b in zip(fixtures,opa,shards):
    assert a['name']==b['name']
    assert a['answers']==b['answers'],(a,b)
    assert [cold[key(call)] for call in case['calls']]==a['expected_pure']
    assert a['entries_after_context_drop']==b['entries_after_context_drop'],(a,b)
    report.append({'name':a['name'],'opa':a['answers'],'shards':b['answers'],'expected_pure':a['expected_pure'],'matches_pure_contract':a['answers']==a['expected_pure'],'entries_after_context_drop':a['entries_after_context_drop']})
retention=[]
a=records(root/'opa.log','REGOCACHE_RETAIN ')
b=records(root/'shards.log','REGOCACHE_RETAIN ')
assert len(a)==len(b)==6,(len(a),len(b))
for left,right in zip(a,b):
    for field in ('kind','calls','declared_max','entries_after_context_drop','entries_after_ordinary_extra'):
        assert left[field]==right[field],(left,right)
    retention.append({'opa':left,'shards':right})
print(json.dumps({'parity':True,'semantic_cases':report,'retention':retention},indent=2))
