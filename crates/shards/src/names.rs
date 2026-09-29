//! Names for containers that `--name` did not name, as dockerd makes them by default: an
//! adjective and a surname joined by an underscore, from word lists of shards' own. The
//! procedure is moby's (docker-v29.8.1, internal/namesgenerator/legacy/names-generator.go
//! and daemon/names.go): from the second try on, a random digit follows; after six names
//! that are taken, the container's short ID names it.

use std::io;

/// Adjectives, each of which describes a person favorably.
const ADJECTIVES: &[&str] = &[
    "able",
    "adept",
    "agile",
    "alert",
    "amazing",
    "amused",
    "ardent",
    "astute",
    "awake",
    "bold",
    "brave",
    "bright",
    "brilliant",
    "busy",
    "calm",
    "candid",
    "careful",
    "cheerful",
    "clever",
    "confident",
    "cool",
    "curious",
    "daring",
    "dazzling",
    "decisive",
    "deft",
    "determined",
    "diligent",
    "dreamy",
    "eager",
    "earnest",
    "elated",
    "elegant",
    "eloquent",
    "epic",
    "exact",
    "fair",
    "fearless",
    "fervent",
    "festive",
    "fond",
    "focused",
    "frank",
    "friendly",
    "gallant",
    "generous",
    "gentle",
    "glad",
    "gracious",
    "happy",
    "hardy",
    "helpful",
    "honest",
    "hopeful",
    "humble",
    "inspired",
    "intent",
    "jolly",
    "jovial",
    "keen",
    "kind",
    "lively",
    "loyal",
    "lucid",
    "merry",
    "modest",
    "musing",
    "nimble",
    "noble",
    "optimistic",
    "patient",
    "peaceful",
    "placid",
    "plucky",
    "poised",
    "precise",
    "proud",
    "quick",
    "quiet",
    "radiant",
    "rapid",
    "ready",
    "resolute",
    "serene",
    "sharp",
    "sincere",
    "sleepy",
    "smart",
    "spirited",
    "steady",
    "stoic",
    "sturdy",
    "sunny",
    "swift",
    "tender",
    "thoughtful",
    "tidy",
    "tranquil",
    "trusty",
    "upbeat",
    "valiant",
    "vibrant",
    "vigilant",
    "vivid",
    "warm",
    "wise",
    "witty",
    "zealous",
];

/// Surnames of people who shaped mathematics, science and computing.
const SURNAMES: &[&str] = &[
    "agnesi",
    "allen",
    "almeida",
    "archimedes",
    "babbage",
    "backus",
    "bardeen",
    "bartik",
    "bell",
    "bernoulli",
    "blackwell",
    "bohr",
    "boole",
    "borg",
    "brahmagupta",
    "brattain",
    "carson",
    "cauchy",
    "cerf",
    "chandrasekhar",
    "chebyshev",
    "clarke",
    "codd",
    "conway",
    "cori",
    "curie",
    "darwin",
    "dijkstra",
    "diophantus",
    "easley",
    "einstein",
    "elion",
    "engelbart",
    "erdos",
    "euclid",
    "euler",
    "faraday",
    "fermat",
    "fermi",
    "feynman",
    "franklin",
    "galileo",
    "galois",
    "gauss",
    "germain",
    "godel",
    "goldberg",
    "goodall",
    "gosling",
    "hamilton",
    "hamming",
    "hawking",
    "heisenberg",
    "hilbert",
    "hodgkin",
    "hollerith",
    "hopper",
    "hoare",
    "huffman",
    "hypatia",
    "jackson",
    "jemison",
    "johnson",
    "kahan",
    "kay",
    "kepler",
    "khayyam",
    "kilby",
    "knuth",
    "kolmogorov",
    "kovalevskaya",
    "lamarr",
    "lamport",
    "laplace",
    "leavitt",
    "lehmann",
    "leibniz",
    "liskov",
    "lovelace",
    "maxwell",
    "mayer",
    "mccarthy",
    "mcclintock",
    "meitner",
    "mendel",
    "mendeleev",
    "milner",
    "minsky",
    "mirzakhani",
    "moore",
    "morse",
    "napier",
    "neumann",
    "newton",
    "nightingale",
    "noether",
    "noyce",
    "ochoa",
    "pascal",
    "pasteur",
    "perlman",
    "pike",
    "planck",
    "poincare",
    "ramanujan",
    "ride",
    "ritchie",
    "rivest",
    "rubin",
    "sagan",
    "sammet",
    "shamir",
    "shannon",
    "shockley",
    "somerville",
    "stallman",
    "sutherland",
    "swartz",
    "tesla",
    "thompson",
    "torvalds",
    "turing",
    "varahamihira",
    "wiles",
    "williams",
    "wilson",
    "wing",
    "wozniak",
    "wright",
    "yalow",
    "yonath",
];

/// A name for try `retry` (0 first), drawn with `random`.
fn draw(retry: u32, random: &mut dyn FnMut(usize) -> io::Result<usize>) -> io::Result<String> {
    let adjective = ADJECTIVES
        .get(random(ADJECTIVES.len())?)
        .copied()
        .unwrap_or("able");
    let surname = SURNAMES.get(random(SURNAMES.len())?).copied().unwrap_or("turing");
    let mut name = format!("{adjective}_{surname}");
    if retry > 0 {
        name.push(char::from(b'0' + (random(10)? as u8)));
    }
    Ok(name)
}

/// A uniform index below `n` from the OS's random source.
fn random(n: usize) -> io::Result<usize> {
    let mut bytes = [0u8; 8];
    shards_vmm::platform::fill_random(&mut bytes)?;
    // n is small, so the bias of a 64-bit modulo is below 2^-50.
    Ok((u64::from_le_bytes(bytes) % n as u64) as usize)
}

/// A name for the container with `id` that `taken` does not refuse.
pub fn generate(id: &str, taken: impl Fn(&str) -> bool) -> io::Result<String> {
    for retry in 0..6 {
        let name = draw(retry, &mut random)?;
        if !taken(&name) {
            return Ok(name);
        }
    }
    Ok(id.get(..12).unwrap_or(id).to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn names_are_an_adjective_and_a_surname_then_a_digit_then_the_id() {
        let name = generate("0123456789abcdef", |_| false).unwrap();
        let (adjective, surname) = name.split_once('_').unwrap();
        assert!(
            ADJECTIVES.contains(&adjective) && SURNAMES.contains(&surname),
            "{name}"
        );
        let second = draw(1, &mut random).unwrap();
        assert!(second.ends_with(|c: char| c.is_ascii_digit()), "{second}");
        assert_eq!(generate("0123456789abcdef", |_| true).unwrap(), "0123456789ab");
        // Every name is one dockerd would accept.
        for a in ADJECTIVES {
            for s in SURNAMES {
                assert!(crate::containers::valid_name(&format!("{a}_{s}9")));
            }
        }
    }
}
