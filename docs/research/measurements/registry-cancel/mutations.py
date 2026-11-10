#!/usr/bin/env python3
"""Remove one real notification, leaving the state publication unchanged."""
import pathlib, sys
source = pathlib.Path('crates/registry/src/http.rs')
text = source.read_text()
if sys.argv[1] == 'completion':
    start = text.index('impl<T> Drop for Reply<T>')
    end = text.index('/// Each wake', start)
    before = '            cancel.rung.notify_all();'
elif sys.argv[1] == 'cancel':
    start = text.index('    pub fn cancel(&self)')
    end = text.index('    /// Ends `helper`', start)
    before = '        self.0.rung.notify_all();'
else:
    raise SystemExit('expected completion or cancel')
part = text[start:end]
assert part.count(before) == 1
part = part.replace(before, '        // MUTATION: notification removed; publication retained.')
source.write_text(text[:start] + part + text[end:])
