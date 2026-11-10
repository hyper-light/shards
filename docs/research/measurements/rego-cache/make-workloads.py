#!/usr/bin/env python3
"""Synthetic experiment sizes only; none becomes runtime policy."""
import json, pathlib, sys
samples, iterations = 20, 100
n=samples*iterations
rows=[
 {'name':'constant_hot','source':'package bench\ndecision := [regex.match("^[a-z]+[0-9]+$", input.value), regex.template_match("cache-<[a-z]+>", input.value, "<", ">")]\n','inputs':[{'value':'cache-alpha'}]*n,'expected':[False,True],'comparison':'equivalent'},
 {'name':'dynamic_regex','source':'package bench\ndecision := regex.match(input.pattern,input.value)\n','inputs':[{'pattern':f'cache-probe-{i}','value':f'cache-probe-{i}'} for i in range(n)],'expected':True,'comparison':'equivalent'},
 {'name':'dynamic_template','source':'package bench\ndecision := regex.template_match(input.pattern,input.value,"<",">")\n','inputs':[{'pattern':f'cache-probe-{i}','value':f'cache-probe-{i}'} for i in range(n)],'expected':True,'comparison':'equivalent'},
 {'name':'dynamic_delimiter','source':'package bench\ndecision := regex.template_match("a<b>",input.value,input.start,input.end)\n','inputs':[{'value':'ab','start':'<' if i%2==0 else '[','end':'>' if i%2==0 else ']'} for i in range(n)],'expected_by_input':[i%2==0 for i in range(n)],'comparison':'upstream_history_deviation'},
 {'name':'failed_compilation','source':'package bench\ndecision := false if { not input.enabled }\ndecision := regex.match("[",input.value) if { input.enabled }\n','inputs':[{'enabled':i%2==1,'value':'x'} for i in range(n)],'comparison':'errors'},
]
path=pathlib.Path(sys.argv[1]);path.parent.mkdir(parents=True,exist_ok=True)
path.write_text(json.dumps({'samples':samples,'iterations':iterations,'workloads':rows},separators=(',',':'))+'\n')
