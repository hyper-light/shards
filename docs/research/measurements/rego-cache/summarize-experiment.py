#!/usr/bin/env python3
"""Actual per-query batch distributions; semantics and bug deviations stay separate."""
import json, math, pathlib, sys
root=pathlib.Path(sys.argv[1])
def distribution(values):
    values=sorted(values)
    return {key:values[max(0,math.ceil(p*len(values))-1)] for key,p in [('p50',.5),('p90',.9),('p99',.99),('max',1)]}
def records(path,prefix):
    return [json.loads(line.split(prefix,1)[1]) for line in path.read_text().splitlines() if prefix in line]
rows=[]
for path in sorted(root.glob('*.log')):
    if path.name=='shards-checks.log':continue
    for row in records(path,'REGOCACHE_EXPERIMENT '):
        measures=row['measurements'];assert len(measures)==20
        stats={'wall_us_per_query':distribution([m['elapsed_ns']/m['iterations']/1000 for m in measures])}
        for field in ('allocated_bytes','allocations','reallocations','retained_delta'):
            if row.get('allocation_profiled',True) and all(field in m['allocation'] for m in measures):stats[field+'_per_query']=distribution([m['allocation'][field]/m['iterations'] for m in measures])
        for field in ('process_cpu_us','minor_faults','major_faults','voluntary_switches','involuntary_switches'):
            if all(field in (m.get('usage') or {}) for m in measures):stats[field+'_per_query']=distribution([m['usage'][field]/m['iterations'] for m in measures])
        row['log']=path.name;row['statistics']=stats;rows.append(row)
assert len(rows)==20,len(rows)
by={(r['mode'],r['workload']):r for r in rows}
comparisons=[]
for workload in ('constant_hot','dynamic_regex','dynamic_template','failed_compilation'):
    opa=by['opa',workload]
    for mode in ('current','uncached','program'):
        other=by[mode,workload]
        assert opa['error_count']==other['error_count'],(workload,mode,opa['error_count'],other['error_count'])
        # Exact text and order are reported, never silently normalized to pass.
        error_match=opa['first_error']==other['first_error'] if opa['first_error'] else other['first_error'] in (None,'')
        values=[json.loads(value) for value in other['first_values']]
        opa_values=[] if opa['first_value'] is None else [opa['first_value']]
        assert values==opa_values,(workload,mode,values,opa_values)
        comparisons.append({'workload':workload,'mode':mode,'first_values_equal':True,'first_error_text_equal':error_match,'error_count':other['error_count'],'opa_over_shards_wall_p50':opa['statistics']['wall_us_per_query']['p50']/other['statistics']['wall_us_per_query']['p50']})
shards_mixed=records(root/'shards-checks.log','REGOCACHE_MIXED_POLICY ')
assert len(shards_mixed)==12
assert all(r['matches_independent_calls'] for r in shards_mixed if r['mode']!='current')
opa_mixed=[]
for path in sorted(root.glob('opa-mixed-*.log')):opa_mixed+=records(path,'REGOCACHE_OPA_MIXED_POLICY ')
assert len(opa_mixed)==4
for current in [r for r in shards_mixed if r['mode']=='current']:
    matched=next(r for r in opa_mixed if r['first']==current['first'] and r['second']==current['second'])
    assert [json.loads(values[0]) for values in current['values']]==matched['values']
error_order=records(root/'shards-checks.log','REGOCACHE_ERROR_ORDER ')
opa_error_order=records(root/'opa-errors.log','REGOCACHE_OPA_ERROR_ORDER ')
assert len(error_order)==3 and len(opa_error_order)==1
assert all(row['errors']==opa_error_order[0]['errors'] for row in error_order)
print(json.dumps({'rows':rows,'equivalent_comparisons':comparisons,'shards_mixed':shards_mixed,'opa_mixed':opa_mixed,'concurrent':records(root/'shards-checks.log','REGOCACHE_CONCURRENT_OWNERS '),'error_order':error_order,'opa_error_order':opa_error_order},indent=2))
