from pathlib import Path
import os,subprocess,json
record=Path.home()/'root-managed-sockaddr-2228-runtime-v2'
cmd=['cargo','test','--locked','-j4','--profile','test-release','-p','shards','--test','managed_network','direct_guest_socket_address_udp4_requires_original_native_tuple','--','--ignored','--exact','--nocapture','--test-threads=1']
results=[]
for mode in ['connected','unconnected']:
 r=record/mode;r.mkdir()
 env=os.environ.copy()
 supplied={'SHARDS_KERNEL':os.environ['SHARDS_KERNEL'],'SHARDS_TEST_KERNEL':os.environ['SHARDS_TEST_KERNEL'],'SHARDS_NETWORK_BOOT_MS':'120000','SHARDS_NETWORK_CASE_MS':'30000','SHARDS_NETWORK_MEMORY_MIB':'256','SHARDS_NETWORK_CPUS':'1','SHARDS_NETWORK_CONFIG_PROVENANCE':'root r122 experimental artifact/output config','SHARDS_NETWORK_EFFECTIVE_CONFIG':str(Path.home()/'managed-kernel-tproxy-5d23-v1/out/config-6.18.48-x86_64-managed-tproxy'),'SHARDS_NETWORK_RECORDS':str(r),'SHARDS_SOCK_ADDR_MODE':mode}
 env.update(supplied)
 (r/'inputs.json').write_text(json.dumps(supplied,indent=2)+'\n');(r/'command.json').write_text(json.dumps(cmd)+'\n')
 with (r/'driver.log').open('wb') as f:p=subprocess.run(cmd,env=env,stdout=f,stderr=subprocess.STDOUT)
 (r/'driver.exit').write_text(str(p.returncode)+'\n')
 stdout=r/f'sock-addr-udp4-{mode}.stdout'
 records=[]
 if stdout.exists():
  for line in stdout.read_text(errors='replace').splitlines():
   if line.startswith(('managed-sock-addr ','managed-intercept ')):
    try:records.append(json.loads(line.split(' ',1)[1]))
    except ValueError:pass
 result={'mode':mode,'cargo_exit':p.returncode,'records':records}
 (r/'observed.json').write_text(json.dumps(result,indent=2)+'\n');results.append(result)
 print(mode,'actual cargo exit',p.returncode,flush=True)
 print(json.dumps(records,indent=2),flush=True)
(record/'results.json').write_text(json.dumps(results,indent=2)+'\n')
