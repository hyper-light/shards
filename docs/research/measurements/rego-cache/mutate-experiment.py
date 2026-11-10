#!/usr/bin/env python3
"""Independent test-only semantic/retention/owner faults. No adoption source."""
import pathlib,sys
path=pathlib.Path('crates/rego/src/funcs/regex.rs')
s=path.read_text();mode=sys.argv[1]
start=s.index('    impl Pool {')
end=s.index('        pub(crate) fn counts',start)
part=s[start:end]
if mode=='kind':
    needle='''            let Some(state) = entries
                .get_mut(&kind)'''
    replacement='''            let lookup_kind = if matches!(kind, Kind::Template(..)) { Kind::Ordinary } else { kind };
            let Some(state) = entries
                .get_mut(&lookup_kind)'''
elif mode=='delimiter':
    needle='''            let Some(state) = entries
                .get_mut(&kind)'''
    replacement='''            let lookup_kind = if matches!(kind, Kind::Template(..)) { Kind::Template(b'<', b'>') } else { kind };
            let Some(state) = entries
                .get_mut(&lookup_kind)'''
elif mode=='history':
    needle='''            let Some(state) = entries'''
    replacement='''            entries.entry(kind).or_default().entry(Rc::from(pattern)).or_insert(State::Uncompiled);
            let Some(state) = entries'''
elif mode=='owner':
    path=pathlib.Path('docs/research/measurements/rego-cache/experiment.rs');s=path.read_text()
    needle='''            drop(program);
            let drop_ns'''
    replacement='''            if let funcs::experiment::Owner::Program(pool) = &program.regex_experiment { std::mem::forget(Rc::clone(pool)); }
            drop(program);
            let drop_ns'''
    assert s.count(needle)==1
    path.write_text(s.replace(needle,replacement));raise SystemExit(0)
else:raise SystemExit('choose kind, delimiter, history or owner')
assert part.count(needle)==1
path.write_text(s[:start]+part.replace(needle,replacement)+s[end:])
