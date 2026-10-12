//! QuickJS-NG, through rquickjs: `create N` (a runtime and context, made and dropped, in
//! µs), `calls` (the cost of a call into the host, in ns, over batches) and `program N`
//! (a runtime made and the program run to its value, in µs).
use std::time::Instant;

use rquickjs::{Context, Function, Object, Promise, Runtime};

fn host(ctx: &rquickjs::Ctx<'_>) {
    let host = Object::new(ctx.clone()).unwrap();
    host.set("whoami", Function::new(ctx.clone(), || "agent-a").unwrap())
        .unwrap();
    host.set(
        "peers",
        Function::new(
            ctx.clone(),
            || r#"[{"name":"b","send":true},{"name":"c","send":false}]"#,
        )
        .unwrap(),
    )
    .unwrap();
    host.set(
        "send",
        Function::new(ctx.clone(), |_to: String, _text: String| 7).unwrap(),
    )
    .unwrap();
    host.set("call", Function::new(ctx.clone(), |x: f64| x).unwrap())
        .unwrap();
    ctx.globals().set("host", host).unwrap();
}

fn main() {
    let (measure, n) = engines::asked();
    let mut values = Vec::new();
    match measure.as_str() {
        "create" => {
            for _ in 0..n {
                let t = Instant::now();
                let rt = Runtime::new().unwrap();
                let ctx = Context::full(&rt).unwrap();
                ctx.with(|ctx| host(&ctx));
                drop(ctx);
                drop(rt);
                values.push(t.elapsed().as_secs_f64() * 1e6);
            }
        }
        "calls" => {
            let rt = Runtime::new().unwrap();
            let ctx = Context::full(&rt).unwrap();
            ctx.with(|ctx| {
                host(&ctx);
                let batch = format!(
                    "(() => {{ let s = 0; for (let i = 0; i < {}; i++) s += host.call(i); return s; }})()",
                    engines::BATCH
                );
                for _ in 0..engines::BATCHES {
                    let t = Instant::now();
                    let _: f64 = ctx.eval(batch.as_str()).unwrap();
                    values.push(t.elapsed().as_secs_f64() * 1e9 / engines::BATCH as f64);
                }
            });
        }
        _ => {
            for _ in 0..n {
                let t = Instant::now();
                let rt = Runtime::new().unwrap();
                let ctx = Context::full(&rt).unwrap();
                let out = ctx.with(|ctx| {
                    host(&ctx);
                    let p: Promise = ctx.eval(engines::PROGRAM).unwrap();
                    p.finish::<String>().unwrap()
                });
                assert!(out.contains("\"total\":7"), "{out}");
                values.push(t.elapsed().as_secs_f64() * 1e6);
            }
        }
    }
    println!(
        "qjs {measure} {} peak_kib={}",
        engines::summary(&mut values),
        engines::peak_kib()
    );
}
