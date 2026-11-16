
/// Appended to crates/shards/tests/api_image.rs for a run of `run.sh` alone, never committed
/// there (D274, PM M604): each memory size's first run, saving its
/// template with its working set recorded, and a run booted alone (a guest its caller gives),
/// each holding 256 MiB to its end.
#[test]
fn zz_sweep_memory() {
    if cannot_run_vms() {
        return;
    }
    let home = common::home_with("api-sweep-home");
    build(&home, "api-sweep:1", &["alloc", "256"], "");
    let sizes: Vec<u64> = std::env::var("SWEEP_MIB")
        .unwrap_or_else(|_| "64".into())
        .split(',')
        .map(|m| m.trim().parse().unwrap())
        .collect();
    for mib in sizes {
        for way in ["save", "boot"] {
            let mut s = spec(&home, "api-sweep:1");
            s.vcpus = 1;
            s.memory_mib = mib;
            if way == "save" {
                s.env
                    .retain(|(n, _)| n != "SHARDS_KERNEL" && n != "SHARDS_INIT");
            }
            let t0 = Instant::now();
            let line = match booted(&ImageShards::new(), &s) {
                Err(e) => {
                    let text = format!("{e:?}");
                    let notable: Vec<&str> = text
                        .split("\\n")
                        .filter(|l| l.contains("SCRATCH") || l.contains("unhandled") || l.contains("ended"))
                        .collect();
                    format!(
                        "BOOT-REFUSED {}\n    {}",
                        text.chars().take(300).collect::<String>(),
                        notable.join("\n    ")
                    )
                }
                Ok(guest) => {
                    let kept = home.join(format!("sweep-{mib}-{way}.stderr"));
                    guest
                        .keep_diagnostics(std::fs::File::create(&kept).unwrap(), u64::MAX)
                        .unwrap();
                    let from = guest.started_from();
                    let out = read_within(&*guest, Channel::Stdout)
                        .map(|b| String::from_utf8_lossy(&b).into_owned());
                    let member = ask(&*guest, |g| g.wait_member(&codex(), Duration::MAX));
                    let _ = guest.stop();
                    let end = ask(&*guest, |g| g.wait_for(Duration::MAX)).map(|e| e.map(|e| e.end));
                    let diag = std::fs::read_to_string(&kept).unwrap_or_default();
                    let notable: Vec<&str> = diag
                        .lines()
                        .filter(|l| l.contains("SCRATCH") || l.contains("unhandled") || l.contains("snapshot"))
                        .take(30)
                        .collect();
                    format!(
                        "from={from:?} out={out:?} member={member:?} end={end:?}\n    {}",
                        notable.join("\n    ")
                    )
                }
            };
            eprintln!("SWEEP mib={mib} way={way} took={:.1?} {line}", t0.elapsed());
        }
    }
}
