#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    EmptyArgv,
    Nul,
    EnvironmentName,
    DuplicateEnvironment,
    Length,
    Capacity,
}

pub(crate) struct Environment<'a> {
    pub name: &'a [u8],
    pub value: &'a [u8],
}

/// Prepared literal bytes and pointer arrays, without any inherited environment.
/// This is serialization only; the actual caller retains execution authority.
pub(crate) struct Literals {
    argv: Vec<Vec<u8>>,
    env: Vec<Vec<u8>>,
    argv_ptrs: Vec<*const libc::c_char>,
    env_ptrs: Vec<*const libc::c_char>,
    bytes: usize,
}

impl Literals {
    pub fn prepare(argv: &[&[u8]], env: &[Environment<'_>]) -> Result<Self, Failure> {
        if argv.is_empty() {
            return Err(Failure::EmptyArgv);
        }
        check_environment(env)?;
        let mut args = Vec::new();
        args.try_reserve_exact(argv.len())
            .map_err(|_| Failure::Capacity)?;
        let mut vars = Vec::new();
        vars.try_reserve_exact(env.len()).map_err(|_| Failure::Capacity)?;
        let mut bytes = 0usize;
        for arg in argv {
            let value = literal(arg, None)?;
            bytes = bytes.checked_add(value.capacity()).ok_or(Failure::Length)?;
            args.push(value);
        }
        for entry in env {
            let value = literal(entry.name, Some(entry.value))?;
            bytes = bytes.checked_add(value.capacity()).ok_or(Failure::Length)?;
            vars.push(value);
        }
        let argv_ptrs = pointers(&args)?;
        let env_ptrs = pointers(&vars)?;
        let pointer_count = argv_ptrs
            .capacity()
            .checked_add(env_ptrs.capacity())
            .ok_or(Failure::Length)?;
        bytes = pointer_count
            .checked_mul(size_of::<*const libc::c_char>())
            .and_then(|n| bytes.checked_add(n))
            .ok_or(Failure::Length)?;
        bytes = args
            .capacity()
            .checked_add(vars.capacity())
            .and_then(|n| n.checked_mul(size_of::<Vec<u8>>()))
            .and_then(|n| bytes.checked_add(n))
            .ok_or(Failure::Length)?;
        Ok(Self {
            argv: args,
            env: vars,
            argv_ptrs,
            env_ptrs,
            bytes,
        })
    }

    pub fn argv(&self) -> *const *const libc::c_char {
        self.argv_ptrs.as_ptr()
    }

    pub fn env(&self) -> *const *const libc::c_char {
        self.env_ptrs.as_ptr()
    }

    /// Actual literal entry counts for the prepared execution's audit facts;
    /// neither the private values nor another retained copy is exposed.
    pub fn counts(&self) -> (usize, usize) {
        (self.argv.len(), self.env.len())
    }

    /// Owned heap regions, including actual vector capacities. Allocator overhead
    /// and transient preparation storage are not included or minted as entitlement.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

fn check_environment(env: &[Environment<'_>]) -> Result<(), Failure> {
    let mut order = Vec::new();
    order
        .try_reserve_exact(env.len())
        .map_err(|_| Failure::Capacity)?;
    for entry in env {
        if entry.name.is_empty() || entry.name.contains(&b'=') {
            return Err(Failure::EnvironmentName);
        }
        order.push(entry.name);
    }
    order.sort_unstable();
    for pair in order.windows(2) {
        if let [left, right] = pair
            && left == right
        {
            return Err(Failure::DuplicateEnvironment);
        }
    }
    Ok(())
}

fn literal(first: &[u8], second: Option<&[u8]>) -> Result<Vec<u8>, Failure> {
    if first.contains(&0) || second.is_some_and(|value| value.contains(&0)) {
        return Err(Failure::Nul);
    }
    let size = first
        .len()
        .checked_add(second.map_or(0, |value| value.len()))
        .and_then(|n| n.checked_add(usize::from(second.is_some())))
        .and_then(|n| n.checked_add(1))
        .ok_or(Failure::Length)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(|_| Failure::Capacity)?;
    bytes.extend_from_slice(first);
    if let Some(second) = second {
        bytes.push(b'=');
        bytes.extend_from_slice(second);
    }
    bytes.push(0);
    Ok(bytes)
}

fn pointers(values: &[Vec<u8>]) -> Result<Vec<*const libc::c_char>, Failure> {
    let size = values.len().checked_add(1).ok_or(Failure::Length)?;
    let mut pointers = Vec::new();
    pointers.try_reserve_exact(size).map_err(|_| Failure::Capacity)?;
    for value in values {
        pointers.push(value.as_ptr().cast());
    }
    pointers.push(std::ptr::null());
    Ok(pointers)
}

impl std::fmt::Debug for Literals {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Literals")
            .field("argc", &self.argv.len())
            .field("envc", &self.env.len())
            .field("retained_bytes", &self.bytes)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_stay_literal_and_environment_is_only_the_explicit_vector() {
        let args: &[&[u8]] = &[b"literal", b"$(not-a-command);*\n", b"", b"\xff"];
        let vars = [Environment {
            name: b"VALUE",
            value: b"$HOME;\xff\n",
        }];
        let prepared = Literals::prepare(args, &vars).unwrap();
        let moved = Box::new(prepared);
        assert_eq!(moved.argv.len(), args.len());
        for (value, expected) in moved.argv.iter().zip(args) {
            assert_eq!(
                std::ffi::CStr::from_bytes_with_nul(value).unwrap().to_bytes(),
                *expected
            );
        }
        assert_eq!(moved.env.first().unwrap(), b"VALUE=$HOME;\xff\n\0");
        assert_eq!(moved.argv_ptrs.last().copied(), Some(std::ptr::null()));
        assert_eq!(moved.env_ptrs.last().copied(), Some(std::ptr::null()));
        assert_eq!(moved.argv(), moved.argv_ptrs.as_ptr());
        assert_eq!(moved.env(), moved.env_ptrs.as_ptr());
        let expected = args.iter().map(|a| a.len() + 1).sum::<usize>()
            + b"VALUE=$HOME;\xff\n".len()
            + 1
            + (args.len() + 1 + vars.len() + 1) * size_of::<*const libc::c_char>();
        assert!(moved.bytes() >= expected + (args.len() + vars.len()) * size_of::<Vec<u8>>());
        let empty = Literals::prepare(&[b"literal"], &[]).unwrap();
        assert!(empty.env.is_empty());
        assert_eq!(empty.env_ptrs, [std::ptr::null()]);
        let debug = format!("{moved:?}");
        assert!(!debug.contains("HOME") && !debug.contains("VALUE"));
    }

    #[test]
    fn invalid_or_ambiguous_vectors_are_typed_refusals() {
        assert!(matches!(Literals::prepare(&[], &[]), Err(Failure::EmptyArgv)));
        assert!(matches!(
            Literals::prepare(&[b"bad\0arg"], &[]),
            Err(Failure::Nul)
        ));
        for vars in [
            [Environment {
                name: b"",
                value: b"x",
            }],
            [Environment {
                name: b"A=B",
                value: b"x",
            }],
        ] {
            assert!(matches!(
                Literals::prepare(&[b"literal"], &vars),
                Err(Failure::EnvironmentName)
            ));
        }
        let nul = [Environment {
            name: b"A",
            value: b"a\0b",
        }];
        assert!(matches!(
            Literals::prepare(&[b"literal"], &nul),
            Err(Failure::Nul)
        ));
        let duplicate = [
            Environment {
                name: b"A",
                value: b"first",
            },
            Environment {
                name: b"A",
                value: b"last",
            },
        ];
        assert!(matches!(
            Literals::prepare(&[b"literal"], &duplicate),
            Err(Failure::DuplicateEnvironment)
        ));
    }
}
