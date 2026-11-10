// Included only as a child of eval in the isolated experiment's cfg(test) build.
use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

struct MeasuredAllocator;
const COUNT_ALLOCATIONS: bool = option_env!("SHARDS_REGO_UNPROFILED").is_none();
static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static FREED: AtomicU64 = AtomicU64::new(0);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);
// SAFETY: every pointer/layout is forwarded unchanged to System. The counters
// allocate nothing and do not dereference pointers. This is test instrumentation.
unsafe impl GlobalAlloc for MeasuredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies GlobalAlloc's valid layout.
        let pointer = unsafe { System.alloc(layout) };
        if COUNT_ALLOCATIONS && !pointer.is_null() {
            ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forward GlobalAlloc's valid layout to the same System operation.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if COUNT_ALLOCATIONS && !pointer.is_null() {
            ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if COUNT_ALLOCATIONS {
            FREED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        // SAFETY: the caller supplies the matching allocation and layout.
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: GlobalAlloc's matching old allocation/layout and new size.
        let next = unsafe { System.realloc(pointer, layout, size) };
        if COUNT_ALLOCATIONS && !next.is_null() {
            FREED.fetch_add(layout.size() as u64, Ordering::Relaxed);
            ALLOCATED.fetch_add(size as u64, Ordering::Relaxed);
            REALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        next
    }
}
#[global_allocator]
static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;

#[derive(Clone, Copy)]
struct Snapshot {
    allocated: u64,
    freed: u64,
    allocs: u64,
    reallocs: u64,
}
fn snapshot() -> Snapshot {
    Snapshot {
        allocated: ALLOCATED.load(Ordering::Relaxed),
        freed: FREED.load(Ordering::Relaxed),
        allocs: ALLOCS.load(Ordering::Relaxed),
        reallocs: REALLOCS.load(Ordering::Relaxed),
    }
}
fn allocation_delta(before: Snapshot, after: Snapshot) -> serde_json::Value {
    serde_json::json!({"allocated_bytes":after.allocated-before.allocated,"freed_bytes":after.freed-before.freed,"allocations":after.allocs-before.allocs,"reallocations":after.reallocs-before.reallocs,"retained_delta":(after.allocated as i128-after.freed as i128)-(before.allocated as i128-before.freed as i128)})
}
#[derive(Clone, Copy)]
struct Usage {
    cpu_us: i64,
    minor: i64,
    major: i64,
    voluntary: i64,
    involuntary: i64,
}
#[cfg(unix)]
fn usage() -> Option<Usage> {
    // SAFETY: zeroed local rusage is valid and getrusage initializes it on success.
    let mut measured = unsafe { std::mem::zeroed::<libc::rusage>() };
    // SAFETY: measured points to a live rusage of the required size.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut measured) } != 0 {
        return None;
    }
    Some(Usage {
        cpu_us: (measured.ru_utime.tv_sec + measured.ru_stime.tv_sec) * 1_000_000
            + measured.ru_utime.tv_usec
            + measured.ru_stime.tv_usec,
        minor: measured.ru_minflt,
        major: measured.ru_majflt,
        voluntary: measured.ru_nvcsw,
        involuntary: measured.ru_nivcsw,
    })
}
#[cfg(not(unix))]
fn usage() -> Option<Usage> {
    None
}
fn usage_delta(before: Option<Usage>, after: Option<Usage>) -> serde_json::Value {
    match (before, after) {
        (Some(a), Some(b)) => {
            serde_json::json!({"process_cpu_us":b.cpu_us-a.cpu_us,"minor_faults":b.minor-a.minor,"major_faults":b.major-a.major,"voluntary_switches":b.voluntary-a.voluntary,"involuntary_switches":b.involuntary-a.involuntary})
        }
        _ => serde_json::Value::Null,
    }
}
struct NoHost;
impl Host for NoHost {
    fn call(&mut self, _: &str, _: &[Value]) -> Result<Option<Value>, HostError> {
        Ok(None)
    }
}
fn query(name: &str) -> Term {
    Term::reference(
        vec![
            Term::var("data", None),
            Term::string("bench", None),
            Term::string(name, None),
        ],
        None,
    )
}
fn compiled(source: &str, mode: &str) -> Program {
    let module = crate::parser::parse_module("probe.rego", source).unwrap();
    let mut compiler = Compiler::new(BTreeMap::from([("probe.rego".into(), module)]), Vec::new(), false);
    compiler.compile();
    assert!(compiler.errors.is_empty(), "{:?}", compiler.errors);
    let mut program = Program::new(&compiler, Vec::new());
    program.regex_experiment = match mode {
        "current" => funcs::experiment::Owner::Current,
        "uncached" => funcs::experiment::Owner::Uncached,
        "program" => funcs::experiment::Owner::Program(funcs::experiment::Pool::from_modules(
            compiler.modules.values(),
        )),
        _ => panic!("unknown mode"),
    };
    drop(compiler);
    program
}
fn evaluated(program: &Program, which: &Term, input: &Value) -> (Vec<Value>, Vec<EvalError>) {
    let mut host = NoHost;
    let mut machine = Machine::new(program, &mut host, funcs::Context::default());
    let values = eval_query(&mut machine, which, Some(input.clone())).unwrap();
    (values, std::mem::take(&mut machine.builtin_errors))
}
fn counts(program: &Program) -> serde_json::Value {
    match &program.regex_experiment {
        funcs::experiment::Owner::Program(pool) => {
            let (slots, ready, failed) = pool.counts();
            serde_json::json!({"slots":slots,"ready":ready,"failed":failed})
        }
        _ => serde_json::json!({"legacy_entries":funcs::experiment::current_entries()}),
    }
}
fn emit(prefix: &str, value: serde_json::Value) {
    println!("{prefix} {value}");
}

#[test]
#[ignore = "isolated Program/Machine compilation ownership measurement"]
fn rego_cache_experiment() {
    let path = std::env::var("SHARDS_REGO_EXPERIMENT_INPUT").unwrap();
    let suite: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let samples = suite["samples"].as_u64().unwrap() as usize;
    let iterations = suite["iterations"].as_u64().unwrap() as usize;
    let decision = query("decision");
    let selected_mode = std::env::var("SHARDS_REGO_EXPERIMENT_MODE").ok();
    let selected_workload = std::env::var("SHARDS_REGO_EXPERIMENT_WORKLOAD").ok();
    for mode in ["current", "uncached", "program"] {
        if selected_mode.as_deref().is_some_and(|selected| selected != mode) {
            continue;
        }
        for workload in suite["workloads"].as_array().unwrap() {
            if selected_workload
                .as_deref()
                .is_some_and(|selected| Some(selected) != workload["name"].as_str())
            {
                continue;
            }
            funcs::experiment::clear_current();
            let inputs: Vec<_> = workload["inputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|input| crate::value::from_json(&input.to_string()).unwrap())
                .collect();
            let before = snapshot();
            let began = Instant::now();
            let program = compiled(workload["source"].as_str().unwrap(), mode);
            let setup_ns = began.elapsed().as_nanos();
            let after_setup = snapshot();
            let owner = match &program.regex_experiment {
                funcs::experiment::Owner::Program(pool) => Some(Rc::downgrade(pool)),
                _ => None,
            };
            let expected = workload
                .get("expected")
                .map(|value| crate::value::from_json(&value.to_string()).unwrap());
            let before_first = snapshot();
            let first_began = Instant::now();
            let first = evaluated(&program, &decision, &inputs[0]);
            let first_ns = first_began.elapsed().as_nanos();
            let after_first = snapshot();
            let after_first_state = counts(&program);
            if workload["comparison"] == "equivalent" {
                assert_eq!(first.0, vec![expected.as_ref().unwrap().clone()]);
                assert!(first.1.is_empty());
            }
            if workload["name"] == "failed_compilation" && mode == "program" {
                assert_eq!(
                    counts(&program),
                    serde_json::json!({"slots":1,"ready":0,"failed":0})
                );
            }
            let mut measurements = Vec::with_capacity(samples);
            let mut semantic_mismatches = 0;
            let mut error_count = 0;
            let mut first_error = None;
            for sample in 0..samples {
                let before_sample = snapshot();
                let usage_before = usage();
                let began = Instant::now();
                for i in 0..iterations {
                    let index = sample * iterations + i;
                    let (values, errors) = evaluated(&program, &decision, &inputs[index]);
                    if workload["comparison"] == "equivalent" {
                        assert!(errors.is_empty());
                        assert_eq!(
                            values.as_slice(),
                            std::slice::from_ref(expected.as_ref().unwrap())
                        );
                    }
                    if workload["comparison"] == "upstream_history_deviation" {
                        let expected = workload["expected_by_input"][index].as_bool().unwrap();
                        semantic_mismatches += usize::from(
                            !matches!(values.as_slice(), [Value::Bool(value)] if *value == expected),
                        );
                    }
                    if !errors.is_empty() {
                        error_count += errors.len();
                        if first_error.is_none() {
                            first_error = Some(errors[0].to_string());
                        }
                    }
                }
                let elapsed_ns = began.elapsed().as_nanos();
                let usage_after = usage();
                let after_sample = snapshot();
                measurements.push(serde_json::json!({"sample":sample,"elapsed_ns":elapsed_ns,"iterations":iterations,"allocation":allocation_delta(before_sample,after_sample),"usage":usage_delta(usage_before,usage_after)}));
            }
            let retained = counts(&program);
            if mode == "program" && workload["name"].as_str().unwrap().starts_with("dynamic_") {
                assert_eq!(
                    retained,
                    serde_json::json!({"slots":0,"ready":0,"failed":0}),
                    "dynamic expression retained compilation history"
                );
            }
            let before_drop = snapshot();
            let began = Instant::now();
            drop(program);
            let drop_ns = began.elapsed().as_nanos();
            let after_drop = snapshot();
            assert!(
                owner.is_none_or(|owner| owner.upgrade().is_none()),
                "program compilation owner outlived Program drop"
            );
            if mode != "current" {
                assert_eq!(funcs::experiment::current_entries(), 0);
            }
            emit(
                "REGOCACHE_EXPERIMENT",
                serde_json::json!({"implementation":"shards","allocation_profiled":COUNT_ALLOCATIONS,"mode":mode,"workload":workload["name"],"comparison":workload["comparison"],"setup_ns":setup_ns,"setup_allocation":allocation_delta(before,after_setup),"first_ns":first_ns,"first_allocation":allocation_delta(before_first,after_first),"state_after_first":after_first_state,"first_values":first.0.iter().map(|v|crate::value::to_json(v).unwrap()).collect::<Vec<_>>(),"first_errors":first.1.iter().map(ToString::to_string).collect::<Vec<_>>(),"measurements":measurements,"retained_state":retained,"semantic_mismatches":semantic_mismatches,"error_count":error_count,"first_error":first_error,"drop_ns":drop_ns,"drop_allocation":allocation_delta(before_drop,after_drop),"program_owner_released":true}),
            );
        }
    }
    funcs::experiment::clear_current();
    if selected_mode.is_none() || std::env::var("SHARDS_REGO_EXPERIMENT_CHECKS").as_deref() == Ok("1") {
        actual_mixed_calls_and_owner_drop();
    }
}

fn actual_mixed_calls_and_owner_drop() {
    let source = "package bench\nordinary := regex.match(\"a.b\",input.value)\ntemplate := regex.template_match(\"a.b\",input.value,\"<\",\">\")\nangle := regex.template_match(\"a<b>\",input.value,\"<\",\">\")\nsquare := regex.template_match(\"a<b>\",input.value,\"[\",\"]\")\n";
    for mode in ["current", "uncached", "program"] {
        for (first, second, input, expected) in [
            ("ordinary", "template", "axb", [true, false]),
            ("template", "ordinary", "axb", [false, true]),
            ("angle", "square", "ab", [true, false]),
            ("square", "angle", "ab", [false, true]),
        ] {
            funcs::experiment::clear_current();
            let program = compiled(source, mode);
            let input = crate::value::from_json(&serde_json::json!({"value":input}).to_string()).unwrap();
            let one = evaluated(&program, &query(first), &input);
            let two = evaluated(&program, &query(second), &input);
            let wanted = vec![vec![Value::Bool(expected[0])], vec![Value::Bool(expected[1])]];
            let got = vec![one.0, two.0];
            if mode != "current" {
                assert_eq!(got, wanted, "constant compilation identity changed policy result");
            }
            emit(
                "REGOCACHE_MIXED_POLICY",
                serde_json::json!({"mode":mode,"first":first,"second":second,"values":got.iter().map(|values|values.iter().map(|value|crate::value::to_json(value).unwrap()).collect::<Vec<_>>()).collect::<Vec<_>>(),"matches_independent_calls":got==wanted,"retained":counts(&program)}),
            );
        }
    }
    let failures = "package bench\ndecision := [regex.match(\"[\",input.value), regex.template_match(\"a<\",input.value,\"<\",\">\")]\nsecond := regex.template_match(\"a<\",input.value,\"<\",\">\")\n";
    let input = crate::value::from_json("{\"value\":\"x\"}").unwrap();
    let mut baseline_errors = None;
    for mode in ["current", "uncached", "program"] {
        funcs::experiment::clear_current();
        let program = compiled(failures, mode);
        if mode == "program" {
            assert_eq!(
                counts(&program),
                serde_json::json!({"slots":2,"ready":0,"failed":0})
            );
        }
        let first = evaluated(&program, &query("decision"), &input);
        assert!(first.0.is_empty());
        assert_eq!(first.1.len(), 1);
        if mode == "program" {
            assert_eq!(
                counts(&program),
                serde_json::json!({"slots":2,"ready":0,"failed":1})
            );
        }
        let second = evaluated(&program, &query("second"), &input);
        assert!(second.0.is_empty());
        assert_eq!(second.1.len(), 1);
        let errors = vec![first.1[0].to_string(), second.1[0].to_string()];
        if let Some(expected) = &baseline_errors {
            assert_eq!(&errors, expected);
        } else {
            baseline_errors = Some(errors.clone());
        }
        emit(
            "REGOCACHE_ERROR_ORDER",
            serde_json::json!({"mode":mode,"errors":errors,"retained":counts(&program)}),
        );
    }
    let barrier = std::sync::Barrier::new(3);
    std::thread::scope(|scope| {
        let (sent, received) = std::sync::mpsc::channel();
        for _ in 0..2 {
            let barrier = &barrier;
            let sent = sent.clone();
            scope.spawn(move || {
                let program = compiled(source, "program");
                let owner = match &program.regex_experiment {
                    funcs::experiment::Owner::Program(pool) => Rc::downgrade(pool),
                    _ => panic!("missing owner"),
                };
                let input = crate::value::from_json("{\"value\":\"axb\"}").unwrap();
                assert_eq!(
                    evaluated(&program, &query("ordinary"), &input).0,
                    vec![Value::Bool(true)]
                );
                sent.send(counts(&program)).unwrap();
                barrier.wait();
                drop(program);
                assert!(owner.upgrade().is_none());
            });
        }
        let one = received.recv().unwrap();
        let two = received.recv().unwrap();
        emit(
            "REGOCACHE_CONCURRENT_OWNERS",
            serde_json::json!({"owners":[one,two],"independent":true}),
        );
        barrier.wait();
    });
}
