"""Instruments the tree for markers.py: run from the repository's root, then build release,
then restore both files (git checkout). Never committed applied."""
p="crates/vmm/src/devices/control.rs"
s=open(p).read()
old="const MARKERS: usize = marker::POWERING_OFF as usize + 1;"
assert s.count(old)==1; s=s.replace(old,"const MARKERS: usize = 16;"); open(p,"w").write(s)
p="crates/init/src/swap.rs"
s=open(p).read()
reps=[('''    let path = format!("/proc/self/fd/{}", node.as_raw_fd());''','''    let _ = crate::linux::control_write(control::MARKER, 7);
    let path = format!("/proc/self/fd/{}", node.as_raw_fd());'''),
('''    drop(device);
    let c = CString::new(path).map_err(|_| "its device's path")?;''','''    drop(device);
    let _ = crate::linux::control_write(control::MARKER, 8);
    let c = CString::new(path).map_err(|_| "its device's path")?;'''),
('''        return Err(format!("swapon: {}", io::Error::last_os_error()));
    }
    Ok(())''','''        return Err(format!("swapon: {}", io::Error::last_os_error()));
    }
    let _ = crate::linux::control_write(control::MARKER, 9);
    Ok(())'''),
]
for a,b in reps:
    assert s.count(a)==1, a[:50]; s=s.replace(a,b)
open(p,"w").write(s)
