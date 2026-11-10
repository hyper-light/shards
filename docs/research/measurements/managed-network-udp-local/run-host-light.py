#!/usr/bin/env python3
"""Compile the exact pure host assertion body; synthetic records are NOT guests."""
import hashlib,json,os,pathlib,subprocess
root=pathlib.Path(os.environ['LIGHT_ROOT'])
records=pathlib.Path(os.environ['RECORDS'])
target=pathlib.Path(os.environ['CARGO_TARGET_DIR'])
assert root.is_absolute() and records.is_absolute() and target.is_absolute()
root.mkdir(); records.mkdir(); target.mkdir()
source=pathlib.Path('crates/shards/tests/managed_network.rs').read_text()
start=source.index('fn assert_udp_records(')
end=source.index('\nfn experimental_tcp_interception(',start)
body=source[start:end]
enum='#[derive(Clone, Copy, Debug)]\nenum InterceptionFamily { Ipv4, Ipv6 }\n'
fixtures=[]
for family in ['Ipv4','Ipv6']:
    for origin in ['local','forward','local-loopback','local-self']:
        for mode in ['connected','unconnected']:
            v6=family=='Ipv6'
            addr=lambda ip,port:f'[{ip}]:{port}' if v6 else f'{ip}:{port}'
            destination_ip=('::1' if v6 else '127.0.0.1') if origin=='local-loopback' else ('2001:db8:1::1' if v6 else '192.0.2.1') if origin=='local-self' else ('2001:db8:1::2' if v6 else '192.0.2.2')
            dst=addr(destination_ip,40001)
            src=('::1' if v6 else '127.0.0.1') if origin=='local-loopback' else ('2001:db8:2::2' if v6 else '198.51.100.2') if origin=='forward' else ('2001:db8:1::1' if v6 else '192.0.2.1')
            baseline,intercepted=addr(src,40002),addr(src,40003)
            relay=addr('::' if v6 else '0.0.0.0',40004)
            origin_tag={'local':'Local','forward':'Forward','local-loopback':'LocalLoopback','local-self':'LocalSelf'}[origin]
            roles=dict(family=family,origin=origin_tag,mode=mode.title(),destination=dst,backend_inode=11 if origin in ('local-loopback','local-self') else 12,relay_inode=11,client_inode=13 if origin=='forward' else 11)
            data=[dict(type='Namespaces',router_inode=11,backend_inode=12,client_inode=13),dict(type='UdpSetup',stage='ReserveReplyTuple',baseline='NotStarted',backend_worker='NotStarted',relay_worker='NotStarted',**roles),dict(type='UdpTarget',reply_local=dst,relay_port=40004,**roles)]
            for role,local,peer,original,binary in [('BaselineClient',baseline,dst,baseline,'4241434b454e4400ff'),('Backend',dst,baseline,dst,'5245515545535400ff'),('InterceptedClient',intercepted,dst,intercepted,'52454c41590080'),('Relay',relay,intercepted,dst,'5245515545535400ff')]:
                for ordinal,step,payload in [(1,'Binary',binary),(2,'Empty','')]:
                    data.append(dict(type='UdpArrival',role=role,local=local,peer=peer,original=original,ordinal=ordinal,step=step,copied_hex=payload,reported=len(payload)//2,flags=0,metadata='Known'))
            for role in ['Backend','Relay']:
                data.append(dict(type='UdpWorkerCompletion',role=role,joined=True,arrivals=2,matched=2,empty=1,terminal='Cancelled' if role=='Backend' else 'ScriptComplete'))
            for role in ['BaselineClient','InterceptedClient']:
                data.append(dict(type='UdpClientOutcome',role=role,stage='ValidateDatagram',outcome='ScriptComplete'))
            data.append(dict(type='UdpIntercepted',joined=True,after_window='Unverified'))
            fixtures.append(dict(family=family,origin=origin,mode=mode,records=data))
(root/'fixtures.json').write_text(json.dumps(fixtures))
(root/'Cargo.toml').write_text('[package]\nname="managed-udp-host-check"\nversion="0.0.0"\nedition="2024"\n[dependencies]\nserde_json="=1.0.151"\n[lib]\npath="lib.rs"\n')
tests=r'''
#[test]
fn typed_datagram_assertions_reject_missing_metadata_effects_and_unknown_join() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("fixtures.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let family = if case["family"] == "Ipv4" { InterceptionFamily::Ipv4 } else { InterceptionFamily::Ipv6 };
        let origin=case["origin"].as_str().unwrap(); let mode=case["mode"].as_str().unwrap();
        let good=case["records"].as_array().unwrap();
        assert_udp_records(good, family, origin, mode);
        // A synthetic syscall error must fail for its typed setup observation,
        // not be accepted as compatibility/interception success.
        let mut refused=good[..2].to_vec();
        refused.push(serde_json::json!({"type":"UdpError","role":"ReplySocket","stage":"ReserveReplyTuple","cause":"Io","kind":"AddrInUse","errno":98}));
        let error=std::panic::catch_unwind(|| assert_udp_records(&refused, family, origin, mode)).unwrap_err();
        let text=error.downcast_ref::<String>().map(String::as_str).or_else(|| error.downcast_ref::<&str>().copied()).unwrap();
        assert!(text.contains("native reply reservation failed; interception was not tested"));
        for field in ["metadata", "reported", "joined", "arrivals", "baseline"] {
            let mut broken=good.clone();
            match field {
                "metadata" => broken.iter_mut().find(|r| r["type"]=="UdpArrival").unwrap()["metadata"]="ShortOriginal".into(),
                "reported" => broken.iter_mut().find(|r| r["type"]=="UdpArrival" && r["step"]=="Empty").unwrap()["reported"]=1.into(),
                "joined" => broken.iter_mut().find(|r| r["type"]=="UdpWorkerCompletion").unwrap()["joined"]=false.into(),
                "baseline" => broken.iter_mut().find(|r| r["type"]=="UdpSetup").unwrap()["baseline"]="Started".into(),
                "arrivals" => broken.iter_mut().find(|r| r["type"]=="UdpWorkerCompletion" && r["role"]=="Backend").unwrap()["arrivals"]=3.into(),
                _ => unreachable!(),
            }
            assert!(std::panic::catch_unwind(|| assert_udp_records(&broken, family, origin, mode)).is_err(), "accepted {field}");
        }
    }
}
'''
(root/'lib.rs').write_text(enum+body+tests)
(records/'source.json').write_text(json.dumps(dict(source='crates/shards/tests/managed_network.rs',source_sha256=hashlib.sha256(source.encode()).hexdigest(),extracted_sha256=hashlib.sha256(body.encode()).hexdigest(),scope='exact pure assertion body only; ignored VM caller/full host crate uncompiled; records synthetic'),indent=2)+'\n')
for name,args in [('host-pure',['test','--','--nocapture']),('host-clippy',['clippy','--tests','--','-D','warnings'])]:
    command=['cargo','+1.98.1',args[0],'--offline','--manifest-path',str(root/'Cargo.toml'),'-j4',*args[1:]]
    (records/(name+'.command')).write_text(json.dumps(command)+'\n')
    with (records/(name+'.log')).open('wb') as out: status=subprocess.call(command,stdout=out,stderr=subprocess.STDOUT)
    (records/(name+'.exit')).write_text(str(status)+'\n')
    print((records/(name+'.log')).read_text())
    if status: raise SystemExit(status)
