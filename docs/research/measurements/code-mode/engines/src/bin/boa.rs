//! Boa: `create N` (a context, made and dropped, in µs), `calls` (the cost of a call into
//! the host, in ns, over batches) and `program N` (a context made and the program run to
//! its value, in µs).
use std::time::Instant;

use boa_engine::builtins::promise::PromiseState;
use boa_engine::object::ObjectInitializer;
use boa_engine::property::Attribute;
use boa_engine::{Context, JsArgs, JsValue, NativeFunction, Source, js_string};

fn host(ctx: &mut Context) {
    let whoami = NativeFunction::from_fn_ptr(|_, _, _| Ok(JsValue::from(js_string!("agent-a"))));
    let peers = NativeFunction::from_fn_ptr(|_, _, _| {
        Ok(JsValue::from(js_string!(
            r#"[{"name":"b","send":true},{"name":"c","send":false}]"#
        )))
    });
    let send = NativeFunction::from_fn_ptr(|_, _, _| Ok(JsValue::from(7)));
    let call = NativeFunction::from_fn_ptr(|_, args, _| Ok(args.get_or_undefined(0).clone()));
    let obj = ObjectInitializer::new(ctx)
        .function(whoami, js_string!("whoami"), 0)
        .function(peers, js_string!("peers"), 0)
        .function(send, js_string!("send"), 2)
        .function(call, js_string!("call"), 1)
        .build();
    ctx.register_global_property(js_string!("host"), obj, Attribute::all())
        .unwrap();
}

fn main() {
    let (measure, n) = engines::asked();
    let mut values = Vec::new();
    match measure.as_str() {
        "create" => {
            for _ in 0..n {
                let t = Instant::now();
                let mut ctx = Context::default();
                host(&mut ctx);
                drop(ctx);
                values.push(t.elapsed().as_secs_f64() * 1e6);
            }
        }
        "calls" => {
            let mut ctx = Context::default();
            host(&mut ctx);
            let batch = format!(
                "(() => {{ let s = 0; for (let i = 0; i < {}; i++) s += host.call(i); return s; }})()",
                engines::BATCH
            );
            for _ in 0..engines::BATCHES {
                let t = Instant::now();
                ctx.eval(Source::from_bytes(&batch)).unwrap();
                values.push(t.elapsed().as_secs_f64() * 1e9 / engines::BATCH as f64);
            }
        }
        _ => {
            for _ in 0..n {
                let t = Instant::now();
                let mut ctx = Context::default();
                host(&mut ctx);
                let v = ctx.eval(Source::from_bytes(engines::PROGRAM)).unwrap();
                ctx.run_jobs().unwrap();
                let out = match v.as_promise().map(|p| p.state()) {
                    Some(PromiseState::Fulfilled(v)) => {
                        v.to_string(&mut ctx).unwrap().to_std_string_escaped()
                    }
                    other => format!("{other:?}"),
                };
                assert!(out.contains("\"total\":7"), "{out}");
                values.push(t.elapsed().as_secs_f64() * 1e6);
            }
        }
    }
    println!(
        "boa {measure} {} peak_kib={}",
        engines::summary(&mut values),
        engines::peak_kib()
    );
}
