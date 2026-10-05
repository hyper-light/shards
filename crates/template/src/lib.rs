//! Go's text/template (Go 1.26), with the functions the Docker CLI adds
//! (docker/cli templates/templates.go), as `--format` runs it.
//!
//! [`Template::parse`] reads a template as docker/cli's `templates.Parse` does
//! (`template.New(name).Funcs(basicFunctions).Parse(text)`, missingkey left at its
//! default), and [`Template::execute`] runs it against a [`Value`] as Go's `Execute`
//! does. Errors are Go's own texts; the Docker CLI adds its prefixes
//! (`template parsing error: …`) itself.
//!
//! Left out, since shards' values cannot hold them: complex numbers, channels, function
//! values (so `call` always fails, as Go's does on a non-function) and uint8 (a string's
//! byte is a uint). Objects print as their `format` says for every verb, and a float has
//! no `%x`. Go's stack grows; this crate's does not, so nesting stops sooner (parse.rs's
//! MAX_DEPTH, exec.rs's MAX_EXEC_DEPTH). Beyond ASCII, letters, digits, printable runes
//! and case come from Rust's Unicode properties, which differ from Go's tables at the
//! margins (strconv.rs, lex.rs, funcs.rs). tests/oracle.rs lists each case of Go's that
//! shards answers differently, and why.

mod exec;
mod fmt;
mod funcs;
mod lex;
mod parse;
mod reflect;
mod strconv;
mod strukt;
mod value;

pub use strukt::Struct;
pub use value::{Kind, Object, Value};

/// A parsed template, with the templates it defines.
#[derive(Debug, Clone)]
pub struct Template {
    name: String,
    set: exec::Set,
    missing_key_error: bool,
}

impl Template {
    /// Parses `text` as the template `name`, with Docker's functions defined. Errors
    /// are text/template's (`template: NAME:LINE: …`).
    pub fn parse(name: &str, text: &str) -> Result<Template, String> {
        let mut lexer = lex::Lexer::new(text);
        let mut trees = parse::Trees::new();
        let defined = |f: &str| funcs::defined(f);
        parse::Parser::new(name, name, &mut lexer, &mut trees, &defined).parse()?;
        Ok(Template {
            name: name.to_owned(),
            set: exec::Set {
                trees,
                text: text.to_owned(),
                parse_name: name.to_owned(),
            },
            missing_key_error: false,
        })
    }

    /// The template with `missingkey=error` (Go's `Option`): a map's missing key is an
    /// error, `map has no entry for key "K"`, where it otherwise reads as no value.
    #[must_use]
    pub fn missing_key_error(&self) -> Template {
        Template {
            missing_key_error: true,
            ..self.clone()
        }
    }

    /// The template's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Runs the template against `data`.
    pub fn execute(&self, data: &Value) -> Result<String, String> {
        let mut out = String::new();
        self.execute_into(data, &mut out)?;
        Ok(out)
    }

    /// Runs the template against `data`, writing to `out`; on an error, what Go would
    /// have written before it stays written.
    pub fn execute_into(&self, data: &Value, out: &mut String) -> Result<(), String> {
        exec::execute(&self.set, &self.name, data, false, self.missing_key_error, out)
    }

    /// Runs the template against a table's header, as docker/cli's formatter does with
    /// `tmpl.Funcs(templates.HeaderFunctions).Execute(…)`: `json`, `split`, `join`,
    /// `title`, `lower`, `upper` and `truncate` return their first argument unchanged.
    pub fn execute_header_into(&self, data: &Value, out: &mut String) -> Result<(), String> {
        exec::execute(&self.set, &self.name, data, true, self.missing_key_error, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, data: &Value) -> Result<String, String> {
        Template::parse("t", text)?.execute(data)
    }

    #[test]
    fn templates_run_as_go_runs_them() {
        let data = Value::map(
            [
                ("Name".to_string(), Value::String("web".into())),
                (
                    "Ports".to_string(),
                    Value::list(vec![Value::Int(80), Value::Int(443)]),
                ),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(run("{{.Name}} {{.Ports}}", &data), Ok("web [80 443]".into()));
        assert_eq!(
            run("{{range $i, $p := .Ports}}{{$i}}={{$p}};{{end}}", &data),
            Ok("0=80;1=443;".into())
        );
        assert_eq!(run("{{.Missing}}", &data), Ok("<no value>".into()));
        assert_eq!(
            run("{{json .}}", &data),
            Ok(r#"{"Name":"web","Ports":[80,443]}"#.into())
        );
        assert_eq!(
            run("{{.Name.X}}", &data),
            Err(
                r#"template: t:1:7: executing "t" at <.Name.X>: can't evaluate field X in type interface {}"#
                    .into()
            )
        );
        assert_eq!(
            run("{{.Name", &data),
            Err("template: t:1: unclosed action".into())
        );
    }

    /// Runs on a 2 MiB stack, Rust's default for a spawned thread: the deepest
    /// expressions, actions and template calls the parser and executor allow.
    #[test]
    fn the_deepest_templates_allowed_fit_a_2_mib_stack() {
        let child = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(|| {
                let n = parse::MAX_DEPTH;
                let parens = format!("{}1{}", "(".repeat(n), ")".repeat(n));
                assert_eq!(run(&format!("{{{{{parens}}}}}"), &Value::Nil), Ok("1".into()));
                let ifs = format!("{}x{}", "{{if 1}}".repeat(n), "{{end}}".repeat(n));
                assert_eq!(run(&ifs, &Value::Nil), Ok("x".into()));
                let too_deep = format!("{{{{(({parens}))}}}}");
                assert!(run(&too_deep, &Value::Nil).is_err_and(|e| e.ends_with("max expression depth exceeded")));
                let nest = |depth: usize| {
                    let mut v = Value::Nil;
                    for _ in 0..depth {
                        v = Value::map([("n".to_string(), v)].into_iter().collect());
                    }
                    v
                };
                // The deepest calls, each evaluating the deepest expression a with allows.
                let deepest = format!("{}.{}", "(".repeat(n - 1), ")".repeat(n - 1));
                let text = format!(r#"{{{{define "r"}}}}{{{{with .n}}}}{{{{$x := {deepest}}}}}{{{{template "r" .}}}}{{{{end}}}}{{{{end}}}}{{{{template "r" .}}}}"#);
                let t = Template::parse("t", &text).unwrap();
                // Each level is a template call and a with.
                let levels = exec::MAX_EXEC_DEPTH / 2;
                assert_eq!(t.execute(&nest(levels)), Ok(String::new()));
                let e = t.execute(&nest(levels + 1)).unwrap_err();
                assert!(e.ends_with("exceeded maximum template depth (200)"), "{e}");
            })
            .unwrap();
        child.join().unwrap();
    }
}
