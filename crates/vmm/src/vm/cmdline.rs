//! Required x86 kernel options stay before Linux's init-argument delimiter.

use crate::arch::x86_64::{boot::BootError, layout};

/// Keeps the existing append order without moving machine options into init's argv.
pub(super) fn with_machine_options<'a>(
    told: &str,
    options: &[Option<&str>],
    bytes: &'a mut [u8; layout::CMDLINE_MAX],
) -> Result<&'a str, BootError> {
    let options = options.iter().flatten();
    let len = options.clone().try_fold(told.len(), |len, option| {
        len.checked_add(1)?.checked_add(option.len())
    });
    let failure = || BootError::DoesNotFit {
        what: "command line",
        at: layout::CMDLINE,
        len: len.map_or(u64::MAX, |len| len as u64),
    };
    let len = len.ok_or_else(failure)?;
    // Match write_boot_state's limit, including room for the terminating NUL.
    if len >= bytes.len()
        || told.as_bytes().contains(&0)
        || options.clone().any(|option| option.as_bytes().contains(&0))
    {
        return Err(failure());
    }
    let mut used = 0usize;
    let mut write = |part: &str| -> Result<(), BootError> {
        let end = used.checked_add(part.len()).ok_or_else(failure)?;
        bytes
            .get_mut(used..end)
            .ok_or_else(failure)?
            .copy_from_slice(part.as_bytes());
        used = end;
        Ok(())
    };
    if let Some(at) = init_delimiter(told) {
        write(told.get(..at).ok_or_else(failure)?)?;
        for option in options {
            write(option)?;
            write(" ")?;
        }
        write(told.get(at..).ok_or_else(failure)?)?;
    } else {
        write(told)?;
        for option in options {
            write(" ")?;
            write(option)?;
        }
    }
    std::str::from_utf8(bytes.get(..used).ok_or_else(failure)?).map_err(|_| failure())
}

#[derive(Clone, Copy)]
enum Quotes {
    Outside,
    Inside,
}

/// Linux v6.18 next_arg/parse_args: only double quotes group bytes; a no-value
/// parameter equal to "--" ends kernel parsing. Do not apply shell escaping or
/// require balanced quotes. The original token and everything after it stay raw.
fn init_delimiter(told: &str) -> Option<usize> {
    let mut bytes = told.bytes().enumerate().peekable();
    while let Some(&(start, byte)) = bytes.peek() {
        if kernel_space(byte) {
            bytes.next();
            continue;
        }
        let mut quotes = if byte == b'"' {
            bytes.next();
            Quotes::Inside
        } else {
            Quotes::Outside
        };
        let mut end = told.len();
        for (at, byte) in bytes.by_ref() {
            match (quotes, byte) {
                (Quotes::Outside, byte) if kernel_space(byte) => {
                    end = at;
                    break;
                }
                (Quotes::Outside, b'"') => quotes = Quotes::Inside,
                (Quotes::Inside, b'"') => quotes = Quotes::Outside,
                _ => {}
            }
        }
        let raw = told.as_bytes().get(start..end)?;
        let param = match raw.strip_prefix(b"\"") {
            Some(param) => param.strip_suffix(b"\"").unwrap_or(param),
            None => raw,
        };
        // Equality also excludes every token carrying an '=' value.
        if param == b"--" {
            return Some(start);
        }
    }
    None
}

fn kernel_space(byte: u8) -> bool {
    // Linux v6.18 lib/ctype.c's _S entries (not Unicode or shell whitespace).
    matches!(byte, b'\t'..=b'\r' | b' ' | 0xa0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delimiter_matches_kernel_quotes_and_values() {
        for (told, delimiter) in [
            ("", None),
            (" \t\n", None),
            ("--", Some(0)),
            ("-- init --", Some(0)),
            ("\"--\" init", Some(0)),
            ("\"--", Some(0)),
            ("\"-- init", None),
            ("\"\" -- init", Some(3)),
            ("\" -- \" -- init", Some(7)),
            ("x=\" -- \" -- init", Some(9)),
            ("x=\" -- init", None),
            ("--=x -- init", Some(5)),
            ("--= -- init", Some(4)),
            ("\"--=\" -- init", Some(6)),
            ("'--' -- init", Some(5)),
            ("\\-- -- init", Some(4)),
            ("\"\"-- -- init", Some(5)),
            ("x\u{000b}-- init", Some(2)),
            ("x\u{00a0}-- init", Some(3)),
            ("--\u{00a0}init", None),
            ("x\u{2003}-- init", None),
            ("λ=\"你好 --\" -- init", Some(15)),
        ] {
            assert_eq!(init_delimiter(told), delimiter, "{told:?}");
        }
    }

    #[test]
    fn required_options_precede_only_the_first_delimiter_and_keep_raw_tail() {
        let options = [Some("reboot=k"), Some("pci=off"), Some("cpuhp.parallel=0")];
        let mut bytes = [0; layout::CMDLINE_MAX];
        for delimiter in ["--", "\"--\""] {
            let prefix = "reboot=b pci=on cpuhp.parallel=1\t";
            let tail = format!("{delimiter}\tmanaged-network  'x y' \"λ\" \\ --\n");
            let told = format!("{prefix}{tail}");
            let actual = with_machine_options(&told, &options, &mut bytes).unwrap();
            assert_eq!(
                actual,
                format!("{prefix}reboot=k pci=off cpuhp.parallel=0 {tail}")
            );
            assert!(actual.ends_with(tail.as_str()));
        }
        assert_eq!(
            with_machine_options("-- init", &options, &mut bytes).unwrap(),
            "reboot=k pci=off cpuhp.parallel=0 -- init"
        );
    }

    #[test]
    fn without_delimiter_existing_order_and_spacing_are_unchanged() {
        let mut bytes = [0; layout::CMDLINE_MAX];
        for (told, options, expected) in [
            ("", [Some("reboot=k"), None, None], " reboot=k"),
            (
                "reboot=b  ",
                [Some("reboot=k"), Some("pci=off"), None],
                "reboot=b   reboot=k pci=off",
            ),
            (
                "x=\"--\" --=value",
                [Some("reboot=k"), None, Some("cpuhp.parallel=0")],
                "x=\"--\" --=value reboot=k cpuhp.parallel=0",
            ),
        ] {
            assert_eq!(
                with_machine_options(told, &options, &mut bytes).unwrap(),
                expected
            );
        }
        assert_eq!(
            with_machine_options("-- init", &[], &mut bytes).unwrap(),
            "-- init"
        );
    }

    #[test]
    fn command_line_max_includes_the_nul_and_overflow_is_typed() {
        let options = [Some("reboot=k"), Some("pci=off")];
        let mut bytes = [0; layout::CMDLINE_MAX];
        let added = " reboot=k pci=off".len();
        for tail in ["", " -- init"] {
            let told = format!(
                "{}{tail}",
                "x".repeat(layout::CMDLINE_MAX - 1 - added - tail.len())
            );
            assert_eq!(
                with_machine_options(&told, &options, &mut bytes).unwrap().len(),
                layout::CMDLINE_MAX - 1
            );
            let too_long = format!("x{told}");
            assert!(matches!(
                with_machine_options(&too_long, &options, &mut bytes),
                Err(BootError::DoesNotFit { what: "command line", at: layout::CMDLINE, len })
                    if len == layout::CMDLINE_MAX as u64
            ));
        }
        assert!(matches!(
            with_machine_options("-- init\0hidden", &options, &mut bytes),
            Err(BootError::DoesNotFit { .. })
        ));
    }
}
