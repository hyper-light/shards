//! The commands there are, as `shards --help` and the management commands' help list
//! them: docker/cli's groups and words for the commands shards serves (cli/cobra.go's
//! usage template, cobra's `rpad`), and shards' own after them.

use std::fmt::Write as _;

use crate::commands;
use crate::flags::Command;

/// A command as a list shows it: its name, and what it does.
#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub name: &'static str,
    pub about: &'static str,
    /// Its own `--help`, where it has one here.
    pub command: Option<&'static Command>,
}

/// cp.go's Short.
const COPY_SHORT: &str = "Copy files/folders between a container and the local filesystem";

/// A heading and what it lists.
#[derive(Debug, Clone, Copy)]
pub struct Group {
    pub heading: &'static str,
    pub entries: &'static [Entry],
}

const fn of(name: &'static str, command: &'static Command) -> Entry {
    Entry {
        name,
        about: command.about,
        command: Some(command),
    }
}

/// A command whose help says more than its line in a list (cobra's Long and Short).
const fn short(name: &'static str, about: &'static str, command: &'static Command) -> Entry {
    Entry {
        name,
        about,
        command: Some(command),
    }
}

const fn own(name: &'static str, about: &'static str) -> Entry {
    Entry {
        name,
        about,
        command: None,
    }
}

/// `build`'s line as docker/cli's root lists it, which is the CLI's, not buildx's.
const BUILD: Entry = Entry {
    name: "build",
    about: "Build an image from a Dockerfile",
    command: Some(&commands::BUILD),
};

/// The root's groups, in docker/cli's order.
pub static TOP: &[Group] = &[
    Group {
        heading: "Common Commands",
        entries: &[
            of("run", &commands::RUN),
            of("exec", &commands::EXEC),
            of("ps", &commands::PS),
            BUILD,
            of("pull", &commands::PULL),
            of("push", &commands::PUSH),
            of("images", &commands::IMAGES),
            own("version", "Show the shards version information"),
        ],
    },
    Group {
        heading: "Management Commands",
        entries: &[
            own("container", "Manage containers"),
            own("image", "Manage images"),
            own("volume", "Manage volumes"),
        ],
    },
    Group {
        heading: "Commands",
        entries: &[
            of("commit", &commands::COMMIT),
            short("cp", COPY_SHORT, &commands::COPY),
            of("create", &commands::CREATE),
            of("diff", &commands::DIFF),
            of("events", &commands::EVENTS),
            of("export", &commands::EXPORT),
            of("history", &commands::HISTORY),
            of("info", &commands::INFO),
            of("inspect", &commands::INSPECT),
            of("kill", &commands::KILL),
            of("load", &commands::LOAD),
            of("logs", &commands::LOGS),
            of("pause", &commands::PAUSE),
            of("port", &commands::PORT),
            of("rename", &commands::RENAME),
            of("restart", &commands::RESTART),
            of("rm", &commands::RM),
            of("rmi", &commands::RMI),
            of("save", &commands::SAVE),
            of("start", &commands::START),
            of("stats", &commands::STATS),
            of("stop", &commands::STOP),
            of("tag", &commands::TAG),
            of("top", &commands::TOP),
            of("unpause", &commands::UNPAUSE),
            of("update", &commands::UPDATE),
            of("wait", &commands::WAIT),
        ],
    },
];

/// The management commands: their name, what they do, and their commands.
pub static MANAGEMENT: &[(&str, &str, &[Entry])] = &[
    (
        "container",
        "Manage containers",
        &[
            of("commit", &commands::COMMIT),
            short("cp", COPY_SHORT, &commands::COPY),
            of("create", &commands::CREATE),
            of("diff", &commands::DIFF),
            of("exec", &commands::EXEC),
            of("export", &commands::EXPORT),
            of("kill", &commands::KILL),
            of("logs", &commands::LOGS),
            of("ls", &commands::PS),
            of("pause", &commands::PAUSE),
            of("port", &commands::PORT),
            of("rename", &commands::RENAME),
            of("restart", &commands::RESTART),
            of("rm", &commands::RM),
            of("run", &commands::RUN),
            of("start", &commands::START),
            of("stats", &commands::STATS),
            of("stop", &commands::STOP),
            of("top", &commands::TOP),
            of("unpause", &commands::UNPAUSE),
            of("update", &commands::UPDATE),
            of("wait", &commands::WAIT),
        ],
    ),
    (
        "image",
        "Manage images",
        &[
            BUILD,
            of("history", &commands::HISTORY),
            of("inspect", &commands::IMAGE_INSPECT),
            of("load", &commands::LOAD),
            of("ls", &commands::IMAGES),
            of("pull", &commands::PULL),
            of("push", &commands::PUSH),
            of("rm", &commands::RMI),
            of("save", &commands::SAVE),
            of("tag", &commands::TAG),
        ],
    ),
    (
        "volume",
        "Manage volumes",
        &[
            of("create", &commands::VOLUME_CREATE),
            of("inspect", &commands::VOLUME_INSPECT),
            of("ls", &commands::VOLUME_LS),
            of("prune", &commands::VOLUME_PRUNE),
            short("rm", "Remove one or more volumes", &commands::VOLUME_RM),
        ],
    ),
];

/// What the root says of shards, where docker/cli says "A self-sufficient runtime for
/// containers".
pub const ABOUT: &str = "Rootless microVMs for agents, built and run like containers";

/// The management command `name`'s line and commands.
pub fn management(name: &str) -> Option<(&'static str, &'static [Entry])> {
    MANAGEMENT
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, about, entries)| (*about, *entries))
}

/// cobra's `rpad` of each name to the widest, at least 11 (its `NamePadding`).
fn listed(text: &mut String, entries: &[Entry]) {
    let pad = entries.iter().map(|e| e.name.len()).max().unwrap_or(0).max(11);
    for e in entries {
        let _ = writeln!(text, "  {:<pad$} {}", e.name, e.about);
    }
}

/// `shards --help`, as docker/cli's root writes its own.
pub fn top() -> String {
    let mut text = format!("Usage:  shards COMMAND\n\n{ABOUT}\n");
    for group in TOP {
        let _ = writeln!(text, "\n{}:", group.heading);
        listed(&mut text, group.entries);
    }
    text.push_str("\nRun 'shards COMMAND --help' for more information on a command.\n");
    text
}

/// `shards NAME --help` of a management command, as docker/cli writes one.
pub fn management_help(name: &str) -> Option<String> {
    let (about, entries) = management(name)?;
    let mut text = format!("Usage:  shards {name} COMMAND\n\n{about}\n\nCommands:\n");
    listed(&mut text, entries);
    let _ = writeln!(
        text,
        "\nRun 'shards {name} COMMAND --help' for more information on a command."
    );
    Some(text)
}

/// A command the root does not have, as docker/cli refuses one (exit status 1).
pub fn unknown(words: &str) -> String {
    format!("shards: unknown command: shards {words}\n\nRun 'shards --help' for more information")
}

/// A command a management command does not have, as docker/cli refuses one.
pub fn unknown_in(name: &str, word: &str) -> String {
    format!(
        "shards: unknown command: shards {name} {word}\n\nUsage:  shards {name}\n\nRun 'shards {name} --help' for more information"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_root_lists_as_docker_lists() {
        let top = top();
        assert!(top.starts_with("Usage:  shards COMMAND\n\n"));
        // cobra's padding: names to 11, then a space.
        assert!(
            top.contains("\nCommon Commands:\n  run         Create and run a new container from an image\n")
        );
        assert!(top.contains("\n  build       Build an image from a Dockerfile\n"));
        assert!(top.ends_with("\nRun 'shards COMMAND --help' for more information on a command.\n"));
    }

    #[test]
    fn management_commands_list_their_own() {
        let image = management_help("image").unwrap();
        assert!(image.starts_with("Usage:  shards image COMMAND\n\nManage images\n\nCommands:\n  build "));
        assert!(image.contains("\n  ls          List images\n"));
        assert!(management_help("network").is_none());
    }

    #[test]
    fn every_listed_command_is_one_shards_reads() {
        for (name, _, entries) in MANAGEMENT {
            for e in *entries {
                // `run` and `exec` the command line reads before the rest (shards' cli).
                if e.command.is_some() && !matches!(e.name, "run" | "exec") {
                    let words = [*name, e.name];
                    assert!(
                        commands::find(&words).is_some() || commands::build(&words).is_some(),
                        "{name} {}",
                        e.name
                    );
                }
            }
        }
    }
}
