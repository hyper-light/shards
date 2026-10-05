//! `version` as docker/cli prints it (cli/command/system/version.go): its versionInfo, as
//! Go types, through its template or the one given, laid out by its tabwriter.

use std::collections::BTreeMap;

use shards_template::{Kind, Struct, Template, Value};

/// The default template, as version.go's defaultVersionTemplate, but for one call:
/// `getDetailsOrder`, the details' keys sorted, is a range over the details, which Go
/// visits in their keys' order, so both write the same.
const TEMPLATE: &str = r#"{{with .Client -}}
Client:{{if ne .Platform nil}}{{if ne .Platform.Name ""}} {{.Platform.Name}}{{end}}{{end}}
 Version:	{{.Version}}
 API version:	{{.APIVersion}}{{if ne .APIVersion .DefaultAPIVersion}} (downgraded from {{.DefaultAPIVersion}}){{end}}
 Go version:	{{.GoVersion}}
 Git commit:	{{.GitCommit}}
 Built:	{{.BuildTime}}
 OS/Arch:	{{.Os}}/{{.Arch}}
 Context:	{{.Context}}
{{- end}}

{{- if ne .Server nil}}{{with .Server}}

Server:{{if ne .Platform.Name ""}} {{.Platform.Name}}{{end}}
 {{- range $component := .Components}}
 {{$component.Name}}:
  {{- if eq $component.Name "Engine" }}
  Version:	{{.Version}}
  API version:	{{index .Details "ApiVersion"}} (minimum version {{index .Details "MinAPIVersion"}})
  Go version:	{{index .Details "GoVersion"}}
  Git commit:	{{index .Details "GitCommit"}}
  Built:	{{index .Details "BuildTime"}}
  OS/Arch:	{{index .Details "Os"}}/{{index .Details "Arch"}}
  Experimental:	{{index .Details "Experimental"}}
  {{- else }}
  Version:	{{$component.Version}}
  {{- range $key, $value := $component.Details}}
  {{$key}}:	{{$value}}
   {{- end}}
  {{- end}}
 {{- end}}
 {{- end}}{{- end}}"#;

/// The client's half (clientVersion).
#[derive(Debug, Clone)]
pub struct Client {
    pub platform: Option<String>,
    pub version: String,
    pub api_version: String,
    pub default_api_version: String,
    pub git_commit: String,
    pub go_version: String,
    pub os: String,
    pub arch: String,
    pub build_time: String,
    pub context: String,
}

/// A part of the server (system.ComponentVersion).
#[derive(Debug, Clone)]
pub struct Component {
    pub name: String,
    pub version: String,
    pub details: BTreeMap<String, String>,
}

/// The server's half (serverVersion), its engine among its components.
#[derive(Debug, Clone)]
pub struct Server {
    pub platform: String,
    pub version: String,
    pub api_version: String,
    pub min_api_version: String,
    pub os: String,
    pub arch: String,
    pub components: Vec<Component>,
}

fn s(v: &str) -> Value {
    Value::String(v.to_owned())
}

fn client(c: &Client) -> Value {
    let platform = match &c.platform {
        Some(name) => Struct::pointer("system.platformInfo")
            .tagged("Name", Some("Name"), true, s(name))
            .value(),
        None => Struct::nil("system.platformInfo"),
    };
    Struct::new("system.clientVersion")
        .tagged("Platform", Some("Platform"), true, platform)
        .tagged("Version", Some("Version"), true, s(&c.version))
        .tagged("APIVersion", Some("ApiVersion"), true, s(&c.api_version))
        .tagged(
            "DefaultAPIVersion",
            Some("DefaultAPIVersion"),
            true,
            s(&c.default_api_version),
        )
        .tagged("GitCommit", Some("GitCommit"), true, s(&c.git_commit))
        .tagged("GoVersion", Some("GoVersion"), true, s(&c.go_version))
        .tagged("Os", Some("Os"), true, s(&c.os))
        .tagged("Arch", Some("Arch"), true, s(&c.arch))
        .tagged("BuildTime", Some("BuildTime"), true, s(&c.build_time))
        .field("Context", s(&c.context))
        .value()
}

fn server(v: &Server) -> Value {
    let engine = v.components.iter().find(|c| c.name == "Engine");
    let detail = |k: &str| engine.and_then(|e| e.details.get(k)).cloned().unwrap_or_default();
    let components: Vec<Value> = v
        .components
        .iter()
        .map(|c| {
            Struct::new("system.ComponentVersion")
                .field("Name", s(&c.name))
                .field("Version", s(&c.version))
                .tagged(
                    "Details",
                    Some("Details"),
                    true,
                    Value::string_map(c.details.iter().map(|(k, v)| (k.clone(), v.clone()))),
                )
                .value()
        })
        .collect();
    Struct::pointer("system.serverVersion")
        .field(
            "Platform",
            Struct::new("client.PlatformInfo")
                .field("Name", s(&v.platform))
                .value(),
        )
        .field("Version", s(&v.version))
        .tagged("APIVersion", Some("ApiVersion"), false, s(&v.api_version))
        .tagged(
            "MinAPIVersion",
            Some("MinAPIVersion"),
            true,
            s(&v.min_api_version),
        )
        .field("Os", s(&v.os))
        .field("Arch", s(&v.arch))
        .tagged(
            "Components",
            Some("Components"),
            true,
            Value::List(Kind::Any, components),
        )
        .tagged("GitCommit", Some("GitCommit"), true, s(&detail("GitCommit")))
        .tagged("GoVersion", Some("GoVersion"), true, s(&detail("GoVersion")))
        .tagged(
            "KernelVersion",
            Some("KernelVersion"),
            true,
            s(&detail("KernelVersion")),
        )
        .tagged(
            "Experimental",
            Some("Experimental"),
            true,
            Value::Bool(detail("Experimental") == "true"),
        )
        .tagged("BuildTime", Some("BuildTime"), true, s(&detail("BuildTime")))
        .value()
}

/// What runVersion writes of `client` and `server` through `format` (`""` for the default,
/// `json`, or a template), or its error and the status it exits with: 64 for a template
/// that does not parse, 1 for one that fails as it runs (after what it wrote).
pub fn render(
    format: &str,
    c: &Client,
    srv: Option<&Server>,
    east_asian: bool,
) -> (String, Option<(u8, String)>) {
    let text = match format {
        "" => TEMPLATE,
        super::JSON => super::JSON_FORMAT,
        other => other,
    };
    let t = match Template::parse("version", text) {
        Ok(t) => t,
        Err(e) => return (String::new(), Some((64, format!("template parsing error: {e}")))),
    };
    let data = Struct::new("system.versionInfo")
        .field("Client", client(c))
        .field(
            "Server",
            srv.map_or_else(|| Struct::nil("system.serverVersion"), server),
        )
        .value();
    let mut raw = String::new();
    let failed = t.execute_into(&data, &mut raw).err();
    // prettyPrintVersion: tabwriter.NewWriter(out, 20, 1, 1, ' ', 0), and a newline.
    raw.push('\n');
    let mut w = super::tabwriter::Writer::new(20, 1, east_asian);
    let mut out = String::new();
    w.write(&raw, &mut out);
    w.flush(&mut out);
    (out, failed.map(|e| (1, e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Docker 29.3.1's own `docker version`, its values given, laid out the same.
    #[test]
    fn version_prints_as_the_docker_cli_prints_it() {
        let c = Client {
            platform: None,
            version: "29.3.1".into(),
            api_version: "1.54".into(),
            default_api_version: "1.54".into(),
            git_commit: "c2be9cc".into(),
            go_version: "go1.25.8".into(),
            os: "darwin".into(),
            arch: "arm64".into(),
            build_time: "Wed Mar 25 16:12:49 2026".into(),
            context: "desktop-linux".into(),
        };
        let engine = Component {
            name: "Engine".into(),
            version: "29.3.1".into(),
            details: [
                ("ApiVersion", "1.54"),
                ("Arch", "arm64"),
                ("BuildTime", "Wed Mar 25 16:14:30 2026"),
                ("Experimental", "false"),
                ("GitCommit", "f78c987"),
                ("GoVersion", "go1.25.8"),
                ("MinAPIVersion", "1.40"),
                ("Os", "linux"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        };
        let containerd = Component {
            name: "containerd".into(),
            version: "v2.2.1".into(),
            details: [(
                "GitCommit".to_owned(),
                "dea7da592f5d1d2b7755e3a161be07f43fad8f75".to_owned(),
            )]
            .into_iter()
            .collect(),
        };
        let srv = Server {
            platform: "Docker Desktop 4.66.1 (222799)".into(),
            version: "29.3.1".into(),
            api_version: "1.54".into(),
            min_api_version: "1.40".into(),
            os: "linux".into(),
            arch: "arm64".into(),
            components: vec![engine, containerd],
        };
        let (out, err) = render("", &c, Some(&srv), false);
        assert_eq!(err, None);
        assert_eq!(
            out,
            "Client:
 Version:           29.3.1
 API version:       1.54
 Go version:        go1.25.8
 Git commit:        c2be9cc
 Built:             Wed Mar 25 16:12:49 2026
 OS/Arch:           darwin/arm64
 Context:           desktop-linux

Server: Docker Desktop 4.66.1 (222799)
 Engine:
  Version:          29.3.1
  API version:      1.54 (minimum version 1.40)
  Go version:       go1.25.8
  Git commit:       f78c987
  Built:            Wed Mar 25 16:14:30 2026
  OS/Arch:          linux/arm64
  Experimental:     false
 containerd:
  Version:          v2.2.1
  GitCommit:        dea7da592f5d1d2b7755e3a161be07f43fad8f75
"
        );
        let (out, _) = render("{{.Server.Version}} {{.Client.Os}}", &c, Some(&srv), false);
        assert_eq!(out, "29.3.1 darwin\n");
        let (_, err) = render("{{.Nope", &c, None, false);
        assert_eq!(err.map(|e| e.0), Some(64));
        let (out, _) = render("{{json .Client.Platform}}", &c, None, false);
        assert_eq!(out, "null\n");
    }
}
