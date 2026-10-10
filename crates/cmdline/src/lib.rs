//! Command lines as the Docker CLI reads them, and the text it answers with, so that
//! shards' commands read and answer as `docker`'s do (docs/design/architecture.md D27):
//! - `buildflags`: what `build`'s `--secret`, `--allow` and `--ulimit` mean, as buildx
//!   and BuildKit read them;
//! - `catalog`: the commands there are, as the root's and management commands' help
//!   list them;
//! - `format`: the formatter's `--format`: tables, templates and JSON of `ps`, `images`,
//!   `stats`, `history` and `system df`;
//! - `flags`: spf13/pflag's parsing, cobra's order of checks, and docker/cli's usage,
//!   help and error texts;
//! - `commands`: the commands, with docker/cli's flags and words;
//! - `grammar`: shards' own `ACTION THING` grammar, said as the commands it runs;
//! - `go`: the Go formats those texts print values in (`strconv`);
//! - `gotime`: Go's time formats, as `logs --since` and `--until` take them;
//! - `network`: `--network`'s attachments, and the checks the CLI makes of them;
//! - `ports`: `-p`'s published ports;
//! - `term`: detach keys, and finding them in a terminal's input;
//! - `width`: how wide the CLI takes text to be on a terminal.
//!
//! Sources: docker/cli v29.8.1 (4a63305d7433) with what it vendors: spf13/pflag v1.0.10,
//! spf13/cobra v1.10.2, mattn/go-runewidth v0.0.29 and golang.org/x/text v0.42.0; and
//! Go 1.26.1, which builds it; moby/term v0.5.2, which it vendors too. `tables` is
//! generated from them by scripts/docker-cli.

pub mod buildcache;
pub mod buildflags;
pub mod catalog;
pub mod commands;
pub mod flags;
pub mod format;
pub mod go;
pub mod gotime;
pub mod grammar;
pub mod mounts;
pub mod network;
pub mod ports;
pub mod resources;
mod tables;
pub mod term;
pub mod width;
