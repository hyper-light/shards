//! Proposed data-only selector for the parent's critical versioned member record.
//! Not registered in the product until ROOT approves the shared module/token.
//! Its tags select pinned protocol schemas; they never issue a scope or grant.

use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeAdapter {
    CodexAppServer0_167_0,
    ClaudeSdkMcp2_1_321,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorError {
    Length { actual: usize },
    Tag { actual: u8 },
    Name,
}

impl fmt::Display for SelectorError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { actual } => write!(out, "native adapter selector length {actual}"),
            Self::Tag { actual } => write!(out, "unsupported native adapter selector tag {actual}"),
            Self::Name => out.write_str("unsupported native adapter selector"),
        }
    }
}

impl core::error::Error for SelectorError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeError {
    Program,
    AppServerRequired,
    PrintRequired,
    UnsupportedOption,
    MissingValue,
    DuplicateMode,
    Transport,
    InputFormat,
    OutputFormat,
    SdkConfig,
    PermissionMode,
    PermissionHandler,
}

impl fmt::Display for ModeError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::Program => "native adapter requires the verified program argv",
            Self::AppServerRequired => "native Codex adapter requires app-server mode",
            Self::PrintRequired => "native Claude adapter requires print mode",
            Self::UnsupportedOption => "unsupported native protocol-mode option",
            Self::MissingValue => "native protocol-mode option requires a value",
            Self::DuplicateMode => "conflicting native protocol-mode options",
            Self::Transport => "native adapter requires its owned stdio protocol pipe",
            Self::InputFormat => "native Claude adapter requires stream-json input",
            Self::OutputFormat => "native Claude adapter requires stream-json output",
            Self::SdkConfig => "native Claude adapter requires supplied SDK configuration",
            Self::PermissionMode => "unsupported pinned Claude permission-mode value",
            Self::PermissionHandler => "unsupported pinned Claude permission handler",
        })
    }
}

impl core::error::Error for ModeError {}

impl NativeAdapter {
    /// One explicit discriminant in the parent's version1 critical member record.
    /// Local wire assignment1/2 identifies the exact pinned protocol versions.
    pub const LEN: usize = size_of::<u8>();

    pub fn encode(self) -> [u8; Self::LEN] {
        [match self {
            Self::CodexAppServer0_167_0 => 1,
            Self::ClaudeSdkMcp2_1_321 => 2,
        }]
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SelectorError> {
        let [tag] = bytes else {
            return Err(SelectorError::Length { actual: bytes.len() });
        };
        match tag {
            1 => Ok(Self::CodexAppServer0_167_0),
            2 => Ok(Self::ClaudeSdkMcp2_1_321),
            _ => Err(SelectorError::Tag { actual: *tag }),
        }
    }

    pub fn parse(name: &str) -> Result<Self, SelectorError> {
        match name {
            "codex-app-server@0.167.0" => Ok(Self::CodexAppServer0_167_0),
            "claude-sdk-mcp@2.1.321" => Ok(Self::ClaudeSdkMcp2_1_321),
            _ => Err(SelectorError::Name),
        }
    }

    /// Necessary literal mode checks for the currently inspected forms, without
    /// changing any argument or interpreting a shell. A successful check is DATA;
    /// SDK config objects, real protocol initialization and all rights remain
    /// independently checked by the owner. Unrecognized forms fail explicitly.
    pub fn check_argv<'a>(self, mut argv: impl Iterator<Item = &'a [u8]>) -> Result<(), ModeError> {
        if argv.next().is_none_or(|program| program.is_empty()) {
            return Err(ModeError::Program);
        }
        match self {
            Self::CodexAppServer0_167_0 => codex(argv),
            Self::ClaudeSdkMcp2_1_321 => claude(argv),
        }
    }
}

fn option(bytes: &[u8]) -> (&[u8], Option<&[u8]>) {
    match bytes.iter().position(|&byte| byte == b'=') {
        Some(at) => match (
            bytes.get(..at),
            at.checked_add(1).and_then(|next| bytes.get(next..)),
        ) {
            (Some(name), Some(value)) => (name, Some(value)),
            _ => (bytes, None),
        },
        None => (bytes, None),
    }
}

fn value<'a>(
    inline: Option<&'a [u8]>,
    argv: &mut impl Iterator<Item = &'a [u8]>,
) -> Result<&'a [u8], ModeError> {
    inline.or_else(|| argv.next()).ok_or(ModeError::MissingValue)
}

fn flag(inline: Option<&[u8]>) -> Result<(), ModeError> {
    if inline.is_some() {
        Err(ModeError::UnsupportedOption)
    } else {
        Ok(())
    }
}

fn codex<'a>(mut argv: impl Iterator<Item = &'a [u8]>) -> Result<(), ModeError> {
    if argv.next() != Some(b"app-server") {
        return Err(ModeError::AppServerRequired);
    }
    let mut transport = None;
    while let Some(arg) = argv.next() {
        let (name, inline) = option(arg);
        match name {
            b"--stdio" => {
                flag(inline)?;
                if transport.replace(()).is_some() {
                    return Err(ModeError::DuplicateMode);
                }
            }
            b"--listen" => {
                if value(inline, &mut argv)? != b"stdio://" {
                    return Err(ModeError::Transport);
                }
                if transport.replace(()).is_some() {
                    return Err(ModeError::DuplicateMode);
                }
            }
            b"-c" | b"--config" | b"--enable" | b"--disable" => {
                value(inline, &mut argv)?;
            }
            b"--strict-config" | b"--analytics-default-enabled" => flag(inline)?,
            _ => return Err(ModeError::UnsupportedOption),
        }
    }
    // Pinned AppServerCommand's actual absent-listen semantics are stdio. This
    // interprets that CLI protocol; it is not an admission/resource default.
    Ok(())
}

fn claude<'a>(mut argv: impl Iterator<Item = &'a [u8]>) -> Result<(), ModeError> {
    if !matches!(argv.next(), Some(b"--print" | b"-p")) {
        return Err(ModeError::PrintRequired);
    }
    let (mut input, mut output, mut config) = (None, None, None);
    while let Some(arg) = argv.next() {
        let (name, inline) = option(arg);
        match name {
            b"--input-format" => {
                if value(inline, &mut argv)? != b"stream-json" {
                    return Err(ModeError::InputFormat);
                }
                if input.replace(()).is_some() {
                    return Err(ModeError::DuplicateMode);
                }
            }
            b"--output-format" => {
                if value(inline, &mut argv)? != b"stream-json" {
                    return Err(ModeError::OutputFormat);
                }
                if output.replace(()).is_some() {
                    return Err(ModeError::DuplicateMode);
                }
            }
            b"--mcp-config" => {
                if value(inline, &mut argv)?.is_empty() {
                    return Err(ModeError::SdkConfig);
                }
                if config.replace(()).is_some() {
                    return Err(ModeError::DuplicateMode);
                }
            }
            b"--permission-prompt-tool" => {
                if value(inline, &mut argv)? != b"stdio" {
                    return Err(ModeError::PermissionHandler);
                }
            }
            b"--permission-prompts" => {
                if !matches!(value(inline, &mut argv)?, b"host" | b"none") {
                    return Err(ModeError::PermissionHandler);
                }
            }
            b"--permission-mode" => {
                if !matches!(
                    value(inline, &mut argv)?,
                    b"acceptEdits" | b"auto" | b"bypassPermissions" | b"manual" | b"dontAsk" | b"plan"
                ) {
                    return Err(ModeError::PermissionMode);
                }
            }
            b"--settings" | b"--setting-sources" => {
                value(inline, &mut argv)?;
            }
            b"--verbose"
            | b"--strict-mcp-config"
            | b"--no-session-persistence"
            | b"--dangerously-skip-permissions"
            | b"--allow-dangerously-skip-permissions" => flag(inline)?,
            _ => return Err(ModeError::UnsupportedOption),
        }
    }
    input.ok_or(ModeError::InputFormat)?;
    output.ok_or(ModeError::OutputFormat)?;
    config.ok_or(ModeError::SdkConfig)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_tags_and_names_never_guess_an_adapter_version() {
        for adapter in [
            NativeAdapter::CodexAppServer0_167_0,
            NativeAdapter::ClaudeSdkMcp2_1_321,
        ] {
            assert_eq!(NativeAdapter::decode(&adapter.encode()), Ok(adapter));
        }
        for bytes in [&[][..], &[1, 0], &[0], &[3], &[u8::MAX]] {
            assert!(NativeAdapter::decode(bytes).is_err());
        }
        assert_eq!(
            NativeAdapter::parse("codex-app-server@0.167.0"),
            Ok(NativeAdapter::CodexAppServer0_167_0)
        );
        assert_eq!(
            NativeAdapter::parse("claude-sdk-mcp@2.1.321"),
            Ok(NativeAdapter::ClaudeSdkMcp2_1_321)
        );
        assert!(NativeAdapter::parse("codex-app-server@0.167.1").is_err());
    }

    #[test]
    fn literal_modes_preserve_args_and_refuse_conflicting_or_non_native_forms() {
        let codex: &[&[u8]] = &[
            b"supplied-binary",
            b"app-server",
            b"--listen",
            b"stdio://",
            b"-c",
            b"cli_auth_credentials_store=\"file\"",
        ];
        assert_eq!(
            NativeAdapter::CodexAppServer0_167_0.check_argv(codex.iter().copied()),
            Ok(())
        );
        for bad in [
            vec![b"codex".as_slice(), b"exec"],
            vec![b"codex".as_slice(), b"app-server", b"--listen", b"unix://"],
            vec![
                b"codex".as_slice(),
                b"app-server",
                b"--stdio",
                b"--listen=stdio://",
            ],
            vec![b"codex".as_slice(), b"app-server", b"proxy"],
        ] {
            assert!(
                NativeAdapter::CodexAppServer0_167_0
                    .check_argv(bad.iter().copied())
                    .is_err()
            );
        }
        let claude: &[&[u8]] = &[
            b"supplied-binary",
            b"--print",
            b"--input-format",
            b"stream-json",
            b"--output-format=stream-json",
            b"--mcp-config",
            b"literal-config.json",
            b"--dangerously-skip-permissions",
        ];
        assert_eq!(
            NativeAdapter::ClaudeSdkMcp2_1_321.check_argv(claude.iter().copied()),
            Ok(())
        );
        assert_eq!(claude.last(), Some(&b"--dangerously-skip-permissions".as_slice()));
        for bad in [
            vec![b"claude".as_slice()],
            vec![b"claude".as_slice(), b"--print", b"--input-format", b"text"],
            vec![
                b"claude".as_slice(),
                b"--print",
                b"--input-format=stream-json",
                b"--input-format=stream-json",
            ],
        ] {
            assert!(
                NativeAdapter::ClaudeSdkMcp2_1_321
                    .check_argv(bad.iter().copied())
                    .is_err()
            );
        }
    }
}
