//! Build checks, as BuildKit's `frontend/dockerfile/linter` runs them: rules, each skipped
//! or enabled by the file's `# check=` directive and by a `# check=` comment just before an
//! instruction, and warnings with BuildKit's names, descriptions, links and messages.

use std::cell::RefCell;

use crate::go;

/// A build check.
#[derive(Debug)]
pub struct Rule {
    pub name: &'static str,
    pub description: &'static str,
    pub url: &'static str,
    pub experimental: bool,
}

pub const STAGE_NAME_CASING: Rule = Rule {
    name: "StageNameCasing",
    description: "Stage names should be lowercase",
    url: "https://docs.docker.com/go/dockerfile/rule/stage-name-casing/",
    experimental: false,
};

pub const FROM_AS_CASING: Rule = Rule {
    name: "FromAsCasing",
    description: "The 'as' keyword should match the case of the 'from' keyword",
    url: "https://docs.docker.com/go/dockerfile/rule/from-as-casing/",
    experimental: false,
};

pub const MAINTAINER_DEPRECATED: Rule = Rule {
    name: "MaintainerDeprecated",
    description: "The MAINTAINER instruction is deprecated, use a label instead to define an image author",
    url: "https://docs.docker.com/go/dockerfile/rule/maintainer-deprecated/",
    experimental: false,
};

pub const INVALID_DEFINITION_DESCRIPTION: Rule = Rule {
    name: "InvalidDefinitionDescription",
    description: "Comment for build stage or argument should follow the format: `# <arg/stage name> <description>`. If this is not intended to be a description comment, add an empty line or comment between the instruction and the comment.",
    url: "https://docs.docker.com/go/dockerfile/rule/invalid-definition-description/",
    experimental: true,
};

/// A warning a rule gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub rule: &'static str,
    pub description: &'static str,
    pub url: &'static str,
    pub message: Vec<u8>,
    pub location: Vec<(usize, usize)>,
}

/// The rules to run, as `# check=` sets them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub skip_all: bool,
    pub skip: Vec<Vec<u8>>,
    pub experimental_all: bool,
    pub experimental: Vec<Vec<u8>>,
    /// `error=true`: warnings fail the build.
    pub error: bool,
}

/// `ParseLintOptions`: `skip=a,b;experimental=c;error=true`, each part at most once.
pub fn parse_options(check: &[u8]) -> Result<Config, Vec<u8>> {
    let check = go::trim_space(check);
    let mut c = Config::default();
    if check.is_empty() {
        return Ok(c);
    }
    // strings.SplitN(checkStr, ";", 3): the third part keeps any further `;`.
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut rest = check;
    while parts.len() < 2 {
        match rest.iter().position(|&b| b == b';') {
            Some(at) => {
                parts.push(go::head(rest, at));
                rest = go::tail(rest, at + 1);
            }
            None => break,
        }
    }
    parts.push(rest);
    let list = |v: &[u8]| -> Vec<Vec<u8>> {
        v.split(|&b| b == b',')
            .map(|r| go::trim_space(r).to_vec())
            .collect()
    };
    for p in parts {
        let Some(at) = p.iter().position(|&b| b == b'=') else {
            return Err([b"invalid check option ".as_slice(), go::quote(p).as_bytes()].concat());
        };
        let k = go::trim_space(go::head(p, at));
        let v = go::trim_space(go::tail(p, at + 1));
        match k {
            b"skip" if v == b"all" => c.skip_all = true,
            b"skip" => c.skip = list(v),
            b"experimental" if v == b"all" => c.experimental_all = true,
            b"experimental" => c.experimental = list(v),
            b"error" => match go::parse_bool(v) {
                Some(b) => c.error = b,
                None => {
                    return Err([
                        b"failed to parse check option ".as_slice(),
                        go::quote(p).as_bytes(),
                        b": strconv.ParseBool: parsing ",
                        go::quote(v).as_bytes(),
                        b": invalid syntax",
                    ]
                    .concat());
                }
            },
            _ => return Err([b"invalid check option ".as_slice(), go::quote(k).as_bytes()].concat()),
        }
    }
    Ok(c)
}

/// Runs rules and keeps their warnings, in order.
#[derive(Debug, Default)]
pub struct Linter {
    pub config: Config,
    warnings: RefCell<Vec<Warning>>,
    /// The rules that warned, for `error=true`.
    called: RefCell<Vec<&'static str>>,
}

impl Linter {
    pub fn new(config: Config) -> Linter {
        Linter {
            config,
            ..Linter::default()
        }
    }

    /// The warnings given so far.
    pub fn warnings(&self) -> Vec<Warning> {
        self.warnings.borrow().clone()
    }

    /// Whether `error=true` turns the warnings into a failure.
    pub fn failed(&self) -> bool {
        self.config.error && !self.called.borrow().is_empty()
    }

    /// This linter with a `# check=` among `comments` merged in:
    /// `WithMergedConfigFromComments`. Only the first such comment counts, and one that
    /// does not parse leaves the configuration as it was.
    pub fn with_comments(&self, comments: &[Vec<u8>]) -> LinterView<'_> {
        for comment in comments {
            let Some((name, value)) = crate::parser::directive_line(comment) else {
                continue;
            };
            if name != b"check" {
                continue;
            }
            let v = value.split(|&b| b == b' ').next().unwrap_or_default();
            return match parse_options(v) {
                Ok(c) => LinterView {
                    linter: self,
                    extra: Some(c),
                },
                Err(_) => LinterView {
                    linter: self,
                    extra: None,
                },
            };
        }
        LinterView {
            linter: self,
            extra: None,
        }
    }

    fn run_with(
        &self,
        extra: Option<&Config>,
        rule: &Rule,
        location: &[(usize, usize)],
        message: Option<&[u8]>,
    ) {
        let c = &self.config;
        let named = |list: &[Vec<u8>]| list.iter().any(|r| r.as_slice() == rule.name.as_bytes());
        if rule.experimental {
            let on = c.experimental_all
                || named(&c.experimental)
                || extra.is_some_and(|e| e.experimental_all || named(&e.experimental));
            if !on {
                return;
            }
        } else {
            let off = c.skip_all || named(&c.skip) || extra.is_some_and(|e| e.skip_all || named(&e.skip));
            if off {
                return;
            }
        }
        self.called.borrow_mut().push(rule.name);
        self.warnings.borrow_mut().push(Warning {
            rule: rule.name,
            description: rule.description,
            url: rule.url,
            message: message.map_or_else(|| rule.description.as_bytes().to_vec(), <[u8]>::to_vec),
            location: location.to_vec(),
        });
    }
}

/// A linter as one instruction sees it, its comments' `# check=` merged in.
#[derive(Debug)]
pub struct LinterView<'a> {
    linter: &'a Linter,
    extra: Option<Config>,
}

impl LinterView<'_> {
    pub fn run(&self, rule: &Rule, location: &[(usize, usize)], message: Option<&[u8]>) {
        self.linter.run_with(self.extra.as_ref(), rule, location, message);
    }
}
