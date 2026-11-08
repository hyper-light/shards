//! URLs as registries send them, in redirects and token realms (shards_registry::url):
//! no panic, what parses prints as text that parses to the same, and joining keeps
//! within what parses.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shards_registry::url::Url;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (base, reference) = text.split_once('\n').unwrap_or((text, ""));
    let Ok(url) = Url::parse(base) else {
        return;
    };
    let again = Url::parse(url.as_str()).expect("a URL's text parses");
    assert_eq!(again.as_str(), url.as_str());
    if let Ok(joined) = url.join(reference) {
        assert!(Url::parse(joined.as_str()).is_ok(), "a joined URL does not parse");
    }
});
