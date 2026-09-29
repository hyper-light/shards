//! ACPI tables for x86_64 guests (ACPI 6.5; research doc §3.5, §4.4). The guest kernel
//! has neither MP-table parsing nor `virtio_mmio.device=`, so these are how it finds its
//! CPUs, the IOAPIC, COM1, the virtio devices, and how it powers off:
//!
//! - RSDP (rev 2) → XSDT → FADT (HW-reduced, rev 6.5) + MADT, FADT → DSDT
//! - MADT: one Local APIC per vCPU (APIC id = index) and the IOAPIC, GSI base 0
//! - DSDT: COM1 (`PNP0501`), each virtio-mmio device (`LNRO0005`), the VM generation ID
//!   (`VMGENCTR`) with the Generic Event Device (`ACPI0013`) that tells the guest of a
//!   new one, and `\_S5`
//! - FADT SLEEP_CONTROL/STATUS registers on a port pair, so `reboot(RB_POWER_OFF)`
//!   writes SLP_TYP 5 | SLP_EN and the VMM sees a power-off

use super::layout;

/// A virtio-mmio transport to describe.
#[derive(Debug, Clone, Copy)]
pub struct MmioDevice {
    pub base: u64,
    pub size: u64,
    pub gsi: u32,
}

#[derive(Debug)]
pub struct Tables {
    /// `(guest-physical address, bytes)` for each table, RSDP included.
    pub blobs: Vec<(u64, Vec<u8>)>,
}

/// SLP_TYP the DSDT's `\_S5` names, and so what the guest writes to power off.
pub const S5_SLP_TYP: u8 = 5;
/// SLEEP_CONTROL_REG's SLP_EN bit (ACPI 6.5 Table 4.19).
pub const SLP_EN: u8 = 1 << 5;

const OEM_ID: &[u8; 6] = b"SHARDS";
const OEM_TABLE: &[u8; 8] = b"SHARDSVM";
const CREATOR: &[u8; 4] = b"SHRD";

fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b)))
}

/// A system description table: the 36-byte header, then `body`; checksummed.
fn sdt(signature: &[u8; 4], revision: u8, body: &[u8]) -> Vec<u8> {
    let len = (36 + body.len()) as u32;
    let mut t = Vec::with_capacity(len as usize);
    t.extend_from_slice(signature);
    t.extend_from_slice(&len.to_le_bytes());
    t.push(revision);
    t.push(0); // checksum, below
    t.extend_from_slice(OEM_ID);
    t.extend_from_slice(OEM_TABLE);
    t.extend_from_slice(&1u32.to_le_bytes()); // OEM revision
    t.extend_from_slice(CREATOR);
    t.extend_from_slice(&1u32.to_le_bytes()); // creator revision
    t.extend_from_slice(body);
    let sum = checksum(&t);
    if let Some(c) = t.get_mut(9) {
        *c = sum;
    }
    t
}

/// A Generic Address Structure for an 8-bit SystemIO register (ACPI 6.5 §5.2.3.2).
fn gas_io8(port: u16) -> [u8; 12] {
    let mut g = [0u8; 12];
    g[0] = 1; // SystemIO
    g[1] = 8; // register bit width
    g[3] = 1; // access size: byte
    g[4..12].copy_from_slice(&u64::from(port).to_le_bytes());
    g
}

/// FADT revision 6.5 (276 bytes), hardware-reduced (ACPI 6.5 §5.2.9, Tables 5.9/5.10).
fn fadt(dsdt: u64) -> Vec<u8> {
    let mut b = vec![0u8; 276 - 36];
    let mut put = |at: usize, bytes: &[u8]| {
        if let Some(dst) = b.get_mut(at - 36..at - 36 + bytes.len()) {
            dst.copy_from_slice(bytes);
        }
    };
    // IAPC_BOOT_ARCH: VGA not present (2), CMOS RTC not present (5). The 8042 bit stays
    // clear, so the guest binds no i8042 driver (research §3.1, §5.6).
    put(109, &((1u16 << 2) | (1 << 5)).to_le_bytes());
    // Flags: PWR_BUTTON (4) and SLP_BUTTON (5), i.e. no fixed buttons; HW_REDUCED_ACPI (20).
    put(112, &((1u32 << 4) | (1 << 5) | (1 << 20)).to_le_bytes());
    put(131, &[5]); // FADT minor version
    put(140, &dsdt.to_le_bytes()); // X_DSDT
    put(244, &gas_io8(layout::ACPI_SLEEP)); // SLEEP_CONTROL_REG
    put(256, &gas_io8(layout::ACPI_SLEEP + 1)); // SLEEP_STATUS_REG
    put(268, b"SHARDSVM"); // hypervisor vendor identity
    sdt(b"FACP", 6, &b)
}

/// MADT (ACPI 6.5 §5.2.12): Local APIC address, no PC-AT dual-8259 flag, one
/// processor Local APIC per vCPU (8-bit ids: at most 254) and the IOAPIC at GSI 0.
fn madt(vcpus: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(layout::LAPIC as u32).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    for i in 0..vcpus {
        let id = i as u8;
        b.extend_from_slice(&[0, 8, id, id]); // type, length, ACPI processor UID, APIC id
        b.extend_from_slice(&1u32.to_le_bytes()); // enabled
    }
    b.extend_from_slice(&[1, 12, 0, 0]); // IOAPIC: type, length, id 0, reserved
    b.extend_from_slice(&(layout::IOAPIC as u32).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes()); // global system interrupt base
    sdt(b"APIC", 6, &b)
}

/// AML (ACPI 6.5 §20): just the terms the DSDT needs.
mod aml {
    /// PkgLength counts its own bytes (§20.2.4).
    pub fn pkg_length(content: usize) -> Vec<u8> {
        let total = [content + 1, content + 2, content + 3, content + 4];
        if total[0] <= 0x3f {
            vec![total[0] as u8]
        } else if total[1] <= 0xfff {
            vec![0x40 | (total[1] & 0xf) as u8, (total[1] >> 4) as u8]
        } else if total[2] <= 0xf_ffff {
            let n = total[2];
            vec![0x80 | (n & 0xf) as u8, (n >> 4) as u8, (n >> 12) as u8]
        } else {
            let n = total[3];
            vec![
                0xc0 | (n & 0xf) as u8,
                (n >> 4) as u8,
                (n >> 12) as u8,
                (n >> 20) as u8,
            ]
        }
    }

    fn with_pkg(op: &[u8], content: Vec<u8>) -> Vec<u8> {
        let mut out = op.to_vec();
        out.extend(pkg_length(content.len()));
        out.extend(content);
        out
    }

    /// A NameSeg: 4 characters, padded with '_'.
    pub fn seg(name: &str) -> [u8; 4] {
        let mut s = [b'_'; 4];
        for (d, c) in s.iter_mut().zip(name.bytes()) {
            *d = c;
        }
        s
    }

    pub fn integer(v: u64) -> Vec<u8> {
        match v {
            0 => vec![0x00],
            1 => vec![0x01],
            v if v <= 0xff => vec![0x0a, v as u8],
            v if v <= 0xffff => [&[0x0b][..], &(v as u16).to_le_bytes()].concat(),
            v if v <= 0xffff_ffff => [&[0x0c][..], &(v as u32).to_le_bytes()].concat(),
            v => [&[0x0e][..], &v.to_le_bytes()].concat(),
        }
    }

    pub fn string(s: &str) -> Vec<u8> {
        let mut out = vec![0x0d];
        out.extend(s.bytes());
        out.push(0);
        out
    }

    /// A compressed EISA id, e.g. "PNP0501" (as ACPICA's EisaId() macro encodes it).
    pub fn eisa_id(id: &str) -> Vec<u8> {
        let b = id.as_bytes();
        let letter = |i: usize| u32::from(b.get(i).copied().unwrap_or(b'@').saturating_sub(0x40)) & 0x1f;
        let hex = |i: usize| b.get(i).and_then(|c| char::from(*c).to_digit(16)).unwrap_or(0);
        let v = letter(0) << 26
            | letter(1) << 21
            | letter(2) << 16
            | hex(3) << 12
            | hex(4) << 8
            | hex(5) << 4
            | hex(6);
        integer(u64::from(v.swap_bytes()))
    }

    /// `Name (NAME, value)` (§20.2.5.1).
    pub fn name(path: &[u8], value: Vec<u8>) -> Vec<u8> {
        let mut out = vec![0x08];
        out.extend_from_slice(path);
        out.extend(value);
        out
    }

    /// `Device (NAME) { ... }` (§20.2.5.2).
    pub fn device(name: &str, body: Vec<u8>) -> Vec<u8> {
        let mut content = seg(name).to_vec();
        content.extend(body);
        with_pkg(&[0x5b, 0x82], content)
    }

    /// `Scope (\NAME) { ... }`.
    pub fn scope_root(name: &str, body: Vec<u8>) -> Vec<u8> {
        let mut content = vec![b'\\'];
        content.extend(seg(name));
        content.extend(body);
        with_pkg(&[0x10], content)
    }

    /// `Package () { ... }` of integers.
    pub fn package(elements: &[u64]) -> Vec<u8> {
        let mut content = vec![elements.len() as u8];
        for &e in elements {
            content.extend(integer(e));
        }
        with_pkg(&[0x12], content)
    }

    /// A ResourceTemplate buffer: the descriptors plus an End Tag (§6.4).
    pub fn resources(descriptors: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes: Vec<u8> = descriptors.concat();
        bytes.extend_from_slice(&[0x79, 0x00]); // End Tag, checksum 0 = "not computed"
        let mut content = integer(bytes.len() as u64);
        content.extend(bytes);
        with_pkg(&[0x11], content)
    }

    /// I/O Port descriptor, 16-bit decode (§6.4.2.5).
    pub fn io(port: u16, len: u8) -> Vec<u8> {
        let mut d = vec![0x47, 0x01];
        d.extend_from_slice(&port.to_le_bytes());
        d.extend_from_slice(&port.to_le_bytes());
        d.extend_from_slice(&[1, len]);
        d
    }

    /// 32-bit Fixed Memory Range descriptor, read/write (§6.4.3.4).
    pub fn memory32_fixed(base: u32, len: u32) -> Vec<u8> {
        let mut d = vec![0x86, 9, 0, 0x01];
        d.extend_from_slice(&base.to_le_bytes());
        d.extend_from_slice(&len.to_le_bytes());
        d
    }

    /// Extended Interrupt descriptor: consumer, edge, active-high, exclusive (§6.4.3.6).
    pub fn interrupt_edge(gsi: u32) -> Vec<u8> {
        let mut d = vec![0x89, 6, 0, 0b0000_0011, 1];
        d.extend_from_slice(&gsi.to_le_bytes());
        d
    }

    /// `\SCOPE.NAME`, from the root: RootChar, DualNamePrefix, two NameSegs (§20.2.2).
    pub fn root_path(scope: &str, name: &str) -> Vec<u8> {
        let mut out = vec![b'\\', 0x2e];
        out.extend(seg(scope));
        out.extend(seg(name));
        out
    }

    /// `Method (NAME, args, Serialized) { ... }` (§20.2.5.2): flags hold the argument
    /// count and, in bit 3, SerializeFlag.
    pub fn method(name: &str, args: u8, body: Vec<u8>) -> Vec<u8> {
        let mut content = seg(name).to_vec();
        content.push((args & 0x7) | 0x08);
        content.extend(body);
        with_pkg(&[0x14], content)
    }

    /// `If (predicate) { ... }` (§20.2.5.3).
    pub fn if_then(predicate: Vec<u8>, body: Vec<u8>) -> Vec<u8> {
        let mut content = predicate;
        content.extend(body);
        with_pkg(&[0xa0], content)
    }

    /// `LEqual (a, b)` (§20.2.5.4).
    pub fn lequal(a: Vec<u8>, b: Vec<u8>) -> Vec<u8> {
        [vec![0x93], a, b].concat()
    }

    /// `ArgN` (§20.2.6.1).
    pub fn arg(n: u8) -> Vec<u8> {
        vec![0x68 + (n & 0x7)]
    }

    /// `Notify (object, value)` (§20.2.5.3).
    pub fn notify(object: Vec<u8>, value: u64) -> Vec<u8> {
        [vec![0x86], object, integer(value)].concat()
    }
}

/// The VM generation ID's device, as Linux's driver finds it (drivers/virt/vmgenid.c:
/// `VMGENCTR`, and `ADDR`, the ID's address as two 32-bit halves), and the Generic Event
/// Device whose interrupt, on [`layout::GSI_GED`], notifies it of a new ID (ACPI 6.5
/// §5.6.9; drivers/acpi/evged.c), as Firecracker declares both.
fn vmgenid_devices(vmgenid: u64) -> Vec<u8> {
    let crs = |descriptors: &[Vec<u8>]| aml::name(b"_CRS", aml::resources(descriptors));
    let mut devices = aml::device(
        "VGEN",
        [
            aml::name(b"_HID", aml::string("VMGENCTR")),
            aml::name(b"_CID", aml::string("VM_Gen_Counter")),
            aml::name(b"_DDN", aml::string("VM_Gen_Counter")),
            aml::name(b"ADDR", aml::package(&[vmgenid & 0xffff_ffff, vmgenid >> 32])),
        ]
        .concat(),
    );
    devices.extend(aml::device(
        "GED",
        [
            aml::name(b"_HID", aml::string("ACPI0013")),
            aml::name(b"_UID", aml::integer(0)),
            crs(&[aml::interrupt_edge(layout::GSI_GED)]),
            aml::method(
                "_EVT",
                1,
                aml::if_then(
                    aml::lequal(aml::arg(0), aml::integer(u64::from(layout::GSI_GED))),
                    aml::notify(aml::root_path("_SB", "VGEN"), 0x80),
                ),
            ),
        ]
        .concat(),
    ));
    devices
}

fn dsdt(virtio: &[MmioDevice], vmgenid: u64) -> Vec<u8> {
    let crs = |descriptors: &[Vec<u8>]| aml::name(b"_CRS", aml::resources(descriptors));
    let mut devices = Vec::new();
    devices.extend(aml::device(
        "COM1",
        [
            aml::name(b"_HID", aml::eisa_id("PNP0501")),
            aml::name(b"_UID", aml::integer(0)),
            crs(&[aml::io(layout::COM1, 8), aml::interrupt_edge(layout::GSI_COM1)]),
        ]
        .concat(),
    ));
    // Device order fixes the guest's /dev/vdX names.
    for (i, d) in virtio.iter().enumerate() {
        devices.extend(aml::device(
            &format!("V{i:03}"),
            [
                aml::name(b"_HID", aml::string("LNRO0005")),
                aml::name(b"_UID", aml::integer(i as u64)),
                aml::name(b"_CCA", aml::integer(1)),
                crs(&[
                    aml::memory32_fixed(d.base as u32, d.size as u32),
                    aml::interrupt_edge(d.gsi),
                ]),
            ]
            .concat(),
        ));
    }
    devices.extend(vmgenid_devices(vmgenid));
    let mut body = aml::scope_root("_SB", devices);
    body.extend(aml::name(b"\\_S5_", aml::package(&[u64::from(S5_SLP_TYP), 0])));
    sdt(b"DSDT", 2, &body)
}

/// Every table, laid out from [`layout::SYSTEM`] with the RSDP at [`layout::RSDP`].
pub fn build(vcpus: u32, virtio: &[MmioDevice]) -> Result<Tables, String> {
    if vcpus == 0 || vcpus > 254 {
        return Err(format!(
            "{vcpus} vCPUs: the MADT holds 8-bit APIC ids, at most 254"
        ));
    }
    // After the VM generation ID, which the VMM writes itself (devices/vmgenid.rs).
    let mut at = layout::VMGENID + crate::devices::vmgenid::SIZE as u64;
    let mut blobs = Vec::new();
    let mut place = |bytes: Vec<u8>| {
        let addr = at.next_multiple_of(8);
        at = addr + bytes.len() as u64;
        blobs.push((addr, bytes));
        addr
    };
    let dsdt_at = place(dsdt(virtio, layout::VMGENID));
    let fadt_at = place(fadt(dsdt_at));
    let madt_at = place(madt(vcpus));
    let mut entries = Vec::new();
    entries.extend_from_slice(&fadt_at.to_le_bytes());
    entries.extend_from_slice(&madt_at.to_le_bytes());
    let xsdt_at = place(sdt(b"XSDT", 1, &entries));
    if at > layout::RSDP {
        return Err("ACPI tables overflow the firmware area".into());
    }

    let mut rsdp = Vec::with_capacity(36);
    rsdp.extend_from_slice(b"RSD PTR ");
    rsdp.push(0); // checksum over the first 20 bytes
    rsdp.extend_from_slice(OEM_ID);
    rsdp.push(2); // revision: ACPI 2.0+
    rsdp.extend_from_slice(&0u32.to_le_bytes()); // RSDT: none, the XSDT only
    rsdp.extend_from_slice(&36u32.to_le_bytes());
    rsdp.extend_from_slice(&xsdt_at.to_le_bytes());
    rsdp.push(0); // extended checksum over all 36 bytes
    rsdp.extend_from_slice(&[0; 3]);
    let first = rsdp.get(..20).map_or(0, checksum);
    if let Some(c) = rsdp.get_mut(8) {
        *c = first;
    }
    let all = checksum(&rsdp);
    if let Some(c) = rsdp.get_mut(32) {
        *c = all;
    }
    blobs.push((layout::RSDP, rsdp));
    Ok(Tables { blobs })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn sum(b: &[u8]) -> u8 {
        b.iter().fold(0u8, |a, &x| a.wrapping_add(x))
    }

    #[test]
    fn every_table_checksums_and_links() {
        let virtio = [MmioDevice {
            base: 0xc000_1000,
            size: 0x200,
            gsi: 5,
        }];
        let t = build(4, &virtio).unwrap();
        let find = |sig: &[u8]| t.blobs.iter().find(|(_, b)| &b[..sig.len()] == sig).unwrap();
        let (rsdp_at, rsdp) = find(b"RSD PTR ");
        assert_eq!(*rsdp_at, layout::RSDP);
        assert_eq!(sum(&rsdp[..20]), 0);
        assert_eq!(sum(rsdp), 0);
        for sig in [&b"XSDT"[..], b"FACP", b"APIC", b"DSDT"] {
            let (_, t) = find(sig);
            assert_eq!(sum(t), 0, "{}", String::from_utf8_lossy(sig));
            assert_eq!(u32::from_le_bytes(t[4..8].try_into().unwrap()) as usize, t.len());
        }
        let xsdt_at = u64::from_le_bytes(rsdp[24..32].try_into().unwrap());
        assert_eq!(find(b"XSDT").0, xsdt_at);
        let (_, fadt) = find(b"FACP");
        assert_eq!(fadt.len(), 276);
        assert_eq!(
            u64::from_le_bytes(fadt[140..148].try_into().unwrap()),
            find(b"DSDT").0
        );
        assert_eq!(fadt[112..116], (0x0010_0030u32).to_le_bytes());
        let (_, madt) = find(b"APIC");
        assert_eq!(madt.len(), 36 + 8 + 4 * 8 + 12);
    }

    #[test]
    fn aml_encodings_match_the_spec() {
        assert_eq!(aml::pkg_length(10), vec![11]);
        assert_eq!(aml::pkg_length(62), vec![63]);
        assert_eq!(aml::pkg_length(63), vec![0x41, 0x04]); // 65 = 0x41
        assert_eq!(aml::pkg_length(0xffd), vec![0x4f, 0xff]);
        assert_eq!(aml::pkg_length(0xffe), vec![0x81, 0x00, 0x01]);
        // EisaId("PNP0501") packs to 0x41D00501 and is stored byte-swapped: 41 D0 05 01.
        assert_eq!(aml::eisa_id("PNP0501"), vec![0x0c, 0x41, 0xd0, 0x05, 0x01]);
        assert_eq!(aml::integer(0x1234), vec![0x0b, 0x34, 0x12]);
        assert_eq!(aml::package(&[5, 0]), vec![0x12, 0x05, 0x02, 0x0a, 0x05, 0x00]);
        // \_SB_.VGEN, and Notify (\_SB_.VGEN, 0x80): as iasl compiles them.
        let path = aml::root_path("_SB", "VGEN");
        assert_eq!(path, b"\\.\x5fSB_VGEN".to_vec());
        assert_eq!(
            aml::notify(path, 0x80),
            [&[0x86][..], b"\\.\x5fSB_VGEN", &[0x0a, 0x80]].concat()
        );
        // Method (_EVT, 1, Serialized) { If (LEqual (Arg0, 23)) { } }
        assert_eq!(
            aml::method(
                "_EVT",
                1,
                aml::if_then(aml::lequal(aml::arg(0), aml::integer(23)), vec![])
            ),
            vec![
                0x14, 0x0c, b'_', b'E', b'V', b'T', 0x09, 0xa0, 0x05, 0x93, 0x68, 0x0a, 23
            ]
        );
    }

    #[test]
    fn the_dsdt_names_the_vm_generation_id() {
        let t = build(1, &[]).unwrap();
        let (_, dsdt) = t.blobs.iter().find(|(_, b)| b.starts_with(b"DSDT")).unwrap();
        let has = |needle: &[u8]| dsdt.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"VMGENCTR\0") && has(b"ACPI0013\0"));
        let addr = [
            aml::integer(layout::VMGENID & 0xffff_ffff),
            aml::integer(layout::VMGENID >> 32),
        ]
        .concat();
        assert!(has(&addr), "ADDR holds {:#x}", layout::VMGENID);
        assert_eq!(layout::VMGENID % 8, 0);
        assert!(
            t.blobs.iter().all(|&(at, _)| at >= layout::VMGENID + 16),
            "no table overlaps the ID"
        );
    }

    #[test]
    fn refuses_what_the_madt_cannot_hold() {
        assert!(build(0, &[]).is_err());
        assert!(build(255, &[]).is_err());
        assert!(build(254, &[]).is_ok());
    }
}
