"""Preserve historical HVF A/B observations without restarting the active VM."""
import hashlib
import json
from pathlib import Path
import plistlib
import re
import shutil
import subprocess

kit=Path('/Users/adalundhe/g121-a4-winarm')
root=Path('/Users/adalundhe/Projects/shards/.claude/worktrees/g121-wa64')
source=kit/'qemu-src'
revision='005c61c0ef6f6ae615283440b4bb7be51b2c6ddd'
assert subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip()==revision
patch=(kit/'evidence/qemu-el2-id.patch').read_bytes()
assert patch==(root/'docs/research/measurements/whp-arm64/nested-host/qemu-hvf-el2.patch').read_bytes()
old=subprocess.check_output(['git','show',revision+':target/arm/hvf/hvf.c'],cwd=source)
needle=b'    if (hvf_nested_virt_enabled()) {\n        /* SME is not implemented'
replacement=b'    if (hvf_nested_virt_enabled()) {\n        /* Local experiment: HVF\'s default ID value hides enabled EL2. */\n        FIELD_DP64_IDREG(&host_isar, ID_AA64PFR0, EL2, 1);\n        /* SME is not implemented'
assert old.count(needle)==1
corrected=old.replace(needle,replacement,1)
def blob(data):return hashlib.sha1(b'blob '+str(len(data)).encode()+b'\0'+data).hexdigest()
assert blob(old).startswith('b00a7e1') and blob(corrected).startswith('bb1a019')
out=root/'docs/research/measurements/whp-arm64/results/hvf-advertisement'
out.mkdir(exist_ok=False)
formats={}
def transcript(path,target):
    raw=path.read_bytes()
    text=raw.decode('utf-8-sig')
    view=('\n'.join(line.rstrip() for line in text.splitlines())+'\n').encode()
    (out/target).parent.mkdir(parents=True,exist_ok=True)
    (out/target).write_bytes(view)
    formats[target]={'capture_sha256':hashlib.sha256(raw).hexdigest(),
                     'display_sha256':hashlib.sha256(view).hexdigest()}
    return text
records=[]
for arm,folder,launch,expected_id,boot in [
    ('A','el2-on','el2-on-launch.txt',43,'11/2/2026 7:58:03 AM'),
    ('B','el2-id','el2-id-launch.txt',26,'11/2/2026 8:23:21 AM'),
]:
    launch_text=transcript(kit/'evidence'/launch,arm+'/launch.txt')
    launch_xml=launch_text[launch_text.index('<?xml'):]
    definition=plistlib.loads(launch_xml.encode())
    windows=transcript(kit/'evidence'/folder/'after-boot.txt',arm+'/windows.txt')
    latest=windows.split('Windows PowerShell transcript start')[-1]
    assert 'LastBootUpTime : '+boot in latest
    events=latest.split('== hypervisor events in System',1)[1].split('== hypervisor event channels',1)[0]
    assert int(re.search(r'Id\s*:\s*(\d+)',events).group(1))==expected_id
    assert 'present=0' in latest and 'WHvCreatePartition: hr=0xC0351000' in latest
    assert 'bits=0x0000000000000AD9' in latest
    records.append({
        'arm':arm,'source_capture_directory':'evidence/'+folder,
        'launch_capture_utc':launch_text.splitlines()[0],
        'program_arguments_recorded':definition['ProgramArguments'],
        'environment_recorded':definition['EnvironmentVariables'],
        'working_directory_recorded':definition['WorkingDirectory'],
        'windows_boot_time_raw':boot,'current_boot_first_hyperv_event':expected_id,
        'whp_present':0,'whp_features':'0x0000000000000AD9',
        'create_partition_hresult':'0xC0351000','vhe_cause':'unestablished',
        'expanded_qemu_argv':'not captured historically; do not treat reconstructed flags as observed argv',
        'historical_binary_sha256':'not captured; current binaries are not historical substitutes',
    })
shutil.copyfile(kit/'start.original.sh',out/'original-start.sh')
(out/'qemu-hvf-el2.patch').write_bytes(patch)
transcript(root/'docs/research/measurements/hvf-el2-vhe/config-result.txt','framework-config.txt')
metadata={
    'scope':'historical EL2-advertisement A/B only; host cause and Step10 unresolved',
    'qemu_tag':'v11.1.3','correction_base_revision':revision,
    'qemu_upstream':'https://gitlab.com/qemu-project/qemu/-/tree/'+revision,
    'patch_sha256':hashlib.sha256(patch).hexdigest(),
    'base_hvf_blob_sha1':blob(old),'corrected_hvf_blob_sha1':blob(corrected),
    'original_start_sha256':hashlib.sha256((out/'original-start.sh').read_bytes()).hexdigest(),
    'clock_note':'Old Windows RTC was interpreted eight hours ahead. Preserve raw guest times; Mac launch UTC is separate.',
    'transcript_note':'Each Windows transcript appends prior boots. Results above use its last transcript block and matching boot.',
    'arms':records,'reboot_or_vm_launch_for_this_preservation':False,
    'native_e2e_qualification':'not established by either HVF arm',
}
(out/'experiment.json').write_text(json.dumps(metadata,indent=2)+'\n')
(out/'transcript-format.json').write_text(json.dumps(formats,indent=2)+'\n')
(out/'README.md').write_text('''# HVF EL2 advertisement A/B: cause unresolved

This preserves the original, unmodified-advertisement run (A) and the locally corrected
PFR0 run (B). It does not establish why Windows' hypervisor failed under HVF, and does
not close Step 10. The later TCG/TF-A host is a separate configuration.

| Arm | Raw Windows boot time | First event from that boot | WHP present | CreatePartition |
|---|---|---|---|---|
| A: default advertisement, nested EL2 enabled | 2026-11-02 07:58:03 | 43: EL2 not present | 0 | 0xC0351000 |
| B: PFR0.EL2 set to 1 when nested EL2 is enabled | 2026-11-02 08:23:21 | 26: BalStatus 0x19, sub-status 0xC | 0 | 0xC0351000 |

Both boots have HypervisorPlatform, VirtualMachinePlatform and Microsoft-Hyper-V-All
enabled and hypervisorlaunchtype Auto. Both return WHP Features 0xad9. Both say
HyperVisorPresent=True through Get-ComputerInfo, which does not prove WHP availability.

The raw Windows transcripts append earlier boots. B therefore still contains older
event43 records; its current boot's event26 must not be replaced with those older
records. The guest's RTC was interpreted eight hours ahead at this stage. These raw
Windows times and the Mac launch UTC timestamps remain separate, without rewriting
either clock or transcript.

QEMU's correction base is v11.1.3, commit
`005c61c0ef6f6ae615283440b4bb7be51b2c6ddd`. The exact two-line correction is
`qemu-hvf-el2.patch`, byte-identical to the retained experiment patch and the fixture's
existing patch. `experiment.json` records its SHA-256 and verifies the old and corrected
HVF source Git blobs against the patch header. It changes PFR0.EL2 only when nested EL2
is enabled; it does not advertise VHE or EL3.

`A/launch.txt` records NESTED=1, CPUS=4 and MEM=8G. The retained original launch script
uses `-M virt,gic-version=3,highmem=on,virtualization=on -accel hvf -cpu host -smp 4 -m 8G`,
with code.fd, vars.fd and win.qcow2. `B/launch.txt` records those environment values plus
ACCEL=hvf, CPU=host, the local QEMU binary path, VM_DISK=win-el2-id.qcow2 and
VARS=vars-el2-id.fd; its binary reported v11.1.3-dirty. The complete launch environments
and script program arguments are retained verbatim.

The expanded QEMU process argument list and historical executable hashes were not
captured. B's historical start-script bytes were not retained separately. Its exact
recorded settings are preserved; a reconstruction from today's script must not be
presented as the observed full command line. Current binaries include later fixture
work and are not substitutes for historical binary hashes. These provenance gaps stay
explicit until a separately labelled rerun can capture them; no rerun interrupted the
active real-VM suite to produce this bundle.

M422's independent framework probe (`framework-config.txt`) measures default PFR0.EL2=0
with nested EL2 enabled, a successful set/readback to1, VH=0 and E2H reading back0. This
advertisement finding prevents assigning Windows' failure to absent VHE alone. The
corrected Windows boot still fails, with an internal loader status rather than a
causally isolated VHE test. The HVF host result and VHE cause remain unresolved.

`A/windows.txt` and `B/windows.txt` preserve the complete selected captures as display
views. `transcript-format.json` records original-capture and display hashes; original
bytes remain in the local kit. Only line endings, BOM and trailing display padding were
normalized. No Windows code or feature check was modified. Neither
arm ran a WHP partition, so neither proves real-VM feature parity or a general host
blocker.
''')
print('Preserved historical HVF A/B, exact patch, recorded flags and complete native outputs; cause unresolved',flush=True)
