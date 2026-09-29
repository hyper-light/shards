//! How wide text is on a terminal, as the Docker CLI measures it: go-runewidth for the
//! tables it aligns (cli/command/formatter/tabwriter), golang.org/x/text/width for the text
//! it cuts short (formatter.Ellipsis). Their tables differ, and the CLI uses both.

use std::cmp::Ordering;

use crate::tables::{DOUBLE, DOUBLE_EAST_ASIAN, WIDE, ZERO};

/// Whether `r` falls in one of `table`'s sorted, inclusive ranges.
pub(crate) fn within(table: &[(u32, u32)], r: u32) -> bool {
    table
        .binary_search_by(|&(first, last)| {
            if last < r {
                Ordering::Less
            } else if first > r {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// Columns go-runewidth gives `c`; `east_asian` for an East Asian locale, where it counts
/// the ambiguous runes double.
pub fn rune_width(c: char, east_asian: bool) -> usize {
    let r = u32::from(c);
    if within(ZERO, r) {
        0
    } else if within(if east_asian { DOUBLE_EAST_ASIAN } else { DOUBLE }, r) {
        2
    } else {
        1
    }
}

/// Columns go-runewidth's `StringWidth` gives `s`, as the CLI's tables measure each cell.
///
/// go-runewidth counts a grapheme cluster as the sum of its runes' widths, at most 2
/// (runewidth.go, graphemeWidth). This counts each rune alone, which agrees except for
/// clusters of several visible runes: emoji joined by U+200D or given a skin tone, flag
/// pairs, and Hangul syllables spelled in jamo.
pub fn string_width(s: &str, east_asian: bool) -> usize {
    if s.is_ascii() {
        return s.bytes().filter(|&b| b >= 0x20 && b != 0x7f).count();
    }
    s.chars().map(|c| rune_width(c, east_asian)).sum()
}

/// Columns golang.org/x/text/width gives `c` for the CLI's `Ellipsis`: two for East Asian
/// Wide and Fullwidth runes, one for the rest.
fn ellipsis_width(c: char) -> usize {
    if within(WIDE, u32::from(c)) { 2 } else { 1 }
}

/// `s` cut to `max` columns, the last of them `…`, as the CLI's `formatter.Ellipsis`
/// (docker/cli cli/command/formatter/displayutils.go) cuts it.
pub fn ellipsis(s: &str, max: usize) -> String {
    const MORE: char = '\u{2026}';
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return s.chars().next().map(String::from).unwrap_or_default();
    }
    if s.is_ascii() {
        if s.len() <= max {
            return s.to_string();
        }
        let mut cut = s.get(..max - 1).unwrap_or_default().to_string();
        cut.push(MORE);
        return cut;
    }
    let mut ends = Vec::new();
    let mut total = 0;
    for c in s.chars() {
        total += ellipsis_width(c);
        ends.push(total);
    }
    if total <= max {
        return s.to_string();
    }
    // The longest start that fits in max - 1 columns, if some rune ends there.
    for (i, pair) in ends.windows(2).enumerate() {
        if let [end, next] = *pair
            && end < max
            && next >= max
        {
            let mut cut: String = s.chars().take(i + 1).collect();
            cut.push(MORE);
            return cut;
        }
    }
    s.to_string()
}

/// Whether go-runewidth takes the locale for East Asian, and so ambiguous runes for two
/// columns (runewidth.go handleEnv; runewidth_posix.go IsEastAsian): RUNEWIDTH_EASTASIAN
/// when set (`1` for yes), else the first of LC_ALL, LC_CTYPE and LANG that is set, by
/// its charset and language. `var` reads the environment.
pub fn east_asian(var: impl Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| var(name).filter(|v| !v.is_empty());
    if let Some(forced) = set("RUNEWIDTH_EASTASIAN") {
        return forced == "1";
    }
    let locale = set("LC_ALL")
        .or_else(|| set("LC_CTYPE"))
        .or_else(|| set("LANG"))
        .unwrap_or_default();
    let bytes = locale.as_bytes();
    if locale == "POSIX" || locale == "C" || matches!(bytes, [b'C', b'.' | b'-', ..]) {
        return false;
    }
    let charset = locale_charset(&locale).unwrap_or(&locale).to_lowercase();
    if charset.ends_with("@cjk_narrow") {
        return false;
    }
    let charset = charset.split('@').next().unwrap_or_default();
    let most = match charset {
        "utf-8" | "utf8" => 6,
        "jis" => 8,
        "eucjp" => 3,
        "euckr" | "euccn" | "sjis" | "cp932" | "cp51932" | "cp936" | "cp949" | "cp950" | "big5" | "gbk"
        | "gb2312" => 2,
        _ => 1,
    };
    most > 1
        && (!charset.starts_with('u')
            || locale.starts_with("ja")
            || locale.starts_with("ko")
            || locale.starts_with("zh"))
}

/// The charset of a locale named `ll.CHARSET` or `ll_CC.CHARSET` (a two- or three-letter
/// language, an optional upper-case country), as go-runewidth's `localeCharset` finds it.
fn locale_charset(locale: &str) -> Option<&str> {
    let language = locale.bytes().take_while(u8::is_ascii_lowercase).count();
    if !(2..=3).contains(&language) {
        return None;
    }
    let mut rest = locale.get(language..)?;
    if let [b'_', a, b, ..] = rest.as_bytes()
        && a.is_ascii_uppercase()
        && b.is_ascii_uppercase()
    {
        rest = rest.get(3..)?;
    }
    rest.strip_prefix('.').filter(|charset| !charset.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ellipses_cut_as_the_cli_cuts() {
        assert_eq!(ellipsis("/bin/testguest sleep", 20), "/bin/testguest sleep");
        assert_eq!(
            ellipsis("/bin/testguest trap INT", 20),
            "/bin/testguest trap\u{2026}"
        );
        assert_eq!(
            ellipsis("日本語日本語日本語日本語", 20),
            "日本語日本語日本語\u{2026}"
        );
        assert_eq!(ellipsis("日本語", 2), "日本語", "no start fits in one column");
        assert_eq!(ellipsis("abc", 1), "a");
        assert_eq!(ellipsis("abc", 0), "");
    }

    #[test]
    fn widths_follow_go_runewidth() {
        assert_eq!(string_width("CONTAINER ID", false), 12);
        assert_eq!(string_width("a\u{2026}", false), 2);
        assert_eq!(string_width("a\u{2026}", true), 3, "the ellipsis is ambiguous");
        assert_eq!(string_width("日本", false), 4);
        assert_eq!(string_width("e\u{301}", false), 1);
        assert_eq!(string_width("\u{7}x", false), 1);
    }

    #[test]
    fn east_asian_locales_are_found_as_go_runewidth_finds_them() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| (*v).to_string())
            }
        };
        assert!(!east_asian(env(&[])));
        assert!(!east_asian(env(&[("LANG", "en_US.UTF-8")])));
        assert!(east_asian(env(&[("LANG", "ja_JP.UTF-8")])));
        assert!(east_asian(env(&[
            ("LC_ALL", "zh_CN.GBK"),
            ("LANG", "en_US.UTF-8")
        ])));
        assert!(!east_asian(env(&[
            ("LC_ALL", "C.UTF-8"),
            ("LANG", "ja_JP.UTF-8")
        ])));
        assert!(!east_asian(env(&[("LANG", "ja_JP.UTF-8@cjk_narrow")])));
        assert!(east_asian(env(&[("RUNEWIDTH_EASTASIAN", "1")])));
        assert!(!east_asian(env(&[
            ("RUNEWIDTH_EASTASIAN", "0"),
            ("LANG", "ja_JP.UTF-8")
        ])));
    }
}
