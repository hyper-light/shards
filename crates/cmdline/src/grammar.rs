//! shards' own grammar, `shards ACTION THING [ARG...]`: what to do, then what to do it to,
//! `vm` (a microVM), `container` or `image`. Each is said again as the command shards
//! runs for it, which is Docker's where Docker has one: `shards stop vm web` is `shards
//! stop web`, `shards remove image alpine` is `shards rmi alpine`. Docker's own words
//! stay what they are, so a script written for `docker` runs unchanged; the grammar is
//! the surface shards documents, and gives every action one shape. Docker's management
//! commands said the other way round, `shards image ls`, are read as `shards ls image`.
//! A kernel is booted with `shards run vm --kernel FILE`, and a snapshot resumed with
//! `shards restore vm DIR`.
//!
//! What a container is to shards: an OCI image, made into a microVM when anything but its
//! removal is asked of it (pull, run, push, import). So `vm` and `container` name the same
//! runs, and `image` what they start from.

/// What an action is done to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thing {
    Vm,
    Container,
    Image,
    /// The daemon that serves runs from warm microVMs.
    Daemon,
    /// The kernel and shards-init runs boot.
    Guest,
    /// What shards takes on disk.
    Disk,
    /// All of shards' own: what prune system clears.
    System,
}

impl Thing {
    fn of(word: &str) -> Option<Thing> {
        match word {
            "vm" | "vms" | "microvm" | "microvms" => Some(Thing::Vm),
            "container" | "containers" => Some(Thing::Container),
            "image" | "images" => Some(Thing::Image),
            "daemon" => Some(Thing::Daemon),
            "guest" => Some(Thing::Guest),
            "disk" => Some(Thing::Disk),
            "system" => Some(Thing::System),
            _ => None,
        }
    }
}

/// `args` said as the command shards runs, if they are in its grammar; `None` if they
/// are not, and are read as they are.
pub fn rewrite(args: &[String]) -> Option<Vec<String>> {
    let (first, second) = (args.first()?.as_str(), args.get(1).map(String::as_str));
    // `shards image ls` is `shards ls image`: Docker's management form, turned round.
    let (action, thing, rest) = match (Thing::of(first), second) {
        (Some(thing @ (Thing::Image | Thing::Container)), Some(action)) => (action, thing, args.get(2..)?),
        (Some(Thing::Vm | Thing::Daemon | Thing::Guest | Thing::Disk | Thing::System), _) => return None,
        _ => (first, Thing::of(second?)?, args.get(2..)?),
    };
    let said: &[&str] = match (action, thing) {
        ("list" | "ls" | "ps", Thing::Vm | Thing::Container) => &["ps"],
        ("list" | "ls", Thing::Image) => &["images"],
        ("remove" | "rm" | "delete", Thing::Vm | Thing::Container) => &["rm"],
        // An image goes with the stopped microVMs made from it.
        ("remove" | "rm" | "delete" | "rmi", Thing::Image) => &["rmi", "--vms"],
        ("inspect", Thing::Image) => &["image", "inspect"],
        ("history", Thing::Image) => &["history"],
        ("inspect", Thing::Disk) => &["system", "df"],
        ("prune", Thing::Vm | Thing::Container) => &["container", "prune"],
        ("prune", Thing::Image) => &["image", "prune"],
        ("prune", Thing::System) => &["system", "prune"],
        ("rename", Thing::Vm | Thing::Container) => &["rename"],
        ("stats" | "watch", Thing::Vm | Thing::Container) => &["stats"],
        ("pause" | "freeze", Thing::Vm | Thing::Container) => &["pause"],
        ("top", Thing::Vm | Thing::Container) => &["top"],
        ("diff" | "changes", Thing::Vm | Thing::Container) => &["diff"],
        ("events" | "watch", Thing::System) => &["events"],
        ("inspect", Thing::System) => &["info"],
        ("export", Thing::Vm | Thing::Container) => &["export"],
        ("start", Thing::Vm | Thing::Container) => &["start"],
        ("restart", Thing::Vm | Thing::Container) => &["restart"],
        ("create" | "make", Thing::Vm | Thing::Container) => &["create"],
        ("unpause" | "resume" | "thaw", Thing::Vm | Thing::Container) => &["unpause"],
        ("inspect", Thing::Vm | Thing::Container) => &["container", "inspect"],
        ("run", Thing::Vm | Thing::Container) => &["run"],
        ("restore", Thing::Vm) => &["restore"],
        ("run" | "start", Thing::Daemon) => &["daemon"],
        ("stop", Thing::Daemon) => &["daemon", "stop"],
        ("configure" | "use" | "set", Thing::Guest) => &["guest", "use"],
        ("inspect" | "show", Thing::Guest) => &["guest"],
        (action @ ("stop" | "kill" | "logs" | "wait" | "port" | "exec"), Thing::Vm | Thing::Container) => {
            return Some(once(action, rest));
        }
        (action @ ("pull" | "push" | "tag" | "save" | "load" | "build"), _) => {
            return Some(once(action, rest));
        }
        _ => return None,
    };
    let mut out: Vec<String> = said.iter().map(|w| (*w).to_string()).collect();
    out.extend(rest.iter().cloned());
    Some(out)
}

fn once(action: &str, rest: &[String]) -> Vec<String> {
    std::iter::once(action.to_string())
        .chain(rest.iter().cloned())
        .collect()
}

/// The sections shards' help lists its actions in, by what they are for, in order.
pub static SECTIONS: &[&str] = &["run", "inspect", "manage", "images"];

/// The actions shards' grammar has, for its help: a line each, its section, the action,
/// the things it acts on, and what it does. How each thing is given is on the action's
/// own page ([`USES`]).
pub static ACTIONS: &[(&str, &str, &str, &str)] = &[
    ("run", "create", "vm", "Make a microVM without starting it"),
    ("run", "exec", "vm", "Run a command in a running microVM"),
    ("run", "restart", "vm", "Stop microVMs, and start them again"),
    ("run", "restore", "vm", "Resume a microVM from a snapshot"),
    (
        "run",
        "run",
        "vm | daemon",
        "Run a command in a new microVM, or the daemon that serves them",
    ),
    (
        "run",
        "start",
        "vm",
        "Start stopped microVMs again, their files as they left them",
    ),
    (
        "inspect",
        "diff",
        "vm",
        "Show what a microVM changed of its image's files",
    ),
    (
        "inspect",
        "events",
        "system",
        "Watch what happens to microVMs and images, as it happens",
    ),
    (
        "inspect",
        "history",
        "image",
        "Show how an image's layers were made",
    ),
    (
        "inspect",
        "inspect",
        "vm | image | disk | guest | system",
        "Show a microVM, an image, disk use, the guest, or shards itself",
    ),
    ("inspect", "list", "vm | image", "List microVMs, or images"),
    ("inspect", "logs", "vm", "Show what a microVM's command printed"),
    (
        "inspect",
        "stats",
        "vm",
        "Watch what microVMs take of the host, live",
    ),
    ("inspect", "top", "vm", "Show a microVM's processes"),
    (
        "manage",
        "configure",
        "guest",
        "Choose the kernel and shards-init runs boot",
    ),
    ("manage", "kill", "vm", "Kill microVMs"),
    ("manage", "pause", "vm", "Freeze microVMs where they are"),
    (
        "manage",
        "prune",
        "vm | image | system",
        "Remove stopped microVMs, unused images, or both",
    ),
    (
        "manage",
        "remove",
        "vm | image",
        "Remove microVMs, or images with their stopped microVMs",
    ),
    ("manage", "rename", "vm", "Name a microVM again"),
    (
        "manage",
        "stop",
        "vm | daemon",
        "Stop microVMs, or the daemon and its runs",
    ),
    ("manage", "tag", "image", "Name an image again"),
    ("manage", "unpause", "vm", "Let frozen microVMs run on"),
    (
        "images",
        "build",
        "image",
        "Build an image from a Dockerfile or Agentfile, and make it a microVM",
    ),
    (
        "images",
        "export",
        "vm",
        "Write a microVM's files out as a tar archive",
    ),
    (
        "images",
        "load",
        "image",
        "Load images from a tar archive or stdin",
    ),
    (
        "images",
        "pull",
        "image",
        "Download an image, and make it a microVM",
    ),
    ("images", "push", "image", "Upload an image to a registry"),
    ("images", "save", "image", "Save images to a tar archive"),
];

/// Each way an action is said, for its own page (`shards run`): its section, the action,
/// what it takes, and what it does.
pub static USES: &[(&str, &str, &str, &str)] = &[
    (
        "run",
        "create",
        "vm IMAGE [COMMAND]",
        "Make a microVM from an image, without starting it",
    ),
    (
        "run",
        "exec",
        "vm NAME COMMAND",
        "Run a command in a running microVM",
    ),
    (
        "run",
        "restart",
        "vm NAME",
        "Stop microVMs: their stop signal, then SIGKILL; and start them again",
    ),
    ("run", "restore", "vm DIR", "Resume a microVM from a snapshot"),
    (
        "run",
        "run",
        "daemon",
        "Serve runs from warm microVMs (the first run starts it)",
    ),
    (
        "run",
        "run",
        "vm --kernel FILE",
        "Boot a kernel directly in a microVM of its own",
    ),
    (
        "run",
        "run",
        "vm IMAGE",
        "Run a command in a new microVM made from an image",
    ),
    (
        "run",
        "start",
        "vm NAME",
        "Start stopped microVMs again, their files as they left them",
    ),
    (
        "inspect",
        "diff",
        "vm NAME",
        "Show what a microVM changed of its image's files",
    ),
    (
        "inspect",
        "events",
        "system",
        "Watch what happens to microVMs and images, as it happens",
    ),
    (
        "inspect",
        "history",
        "image NAME",
        "Show how an image's layers were made",
    ),
    (
        "inspect",
        "inspect",
        "disk",
        "Show what images, microVMs and templates take on disk",
    ),
    (
        "inspect",
        "inspect",
        "guest",
        "Show the kernel and shards-init runs boot",
    ),
    (
        "inspect",
        "inspect",
        "system",
        "Show what shards is, holds and runs on",
    ),
    (
        "inspect",
        "inspect",
        "vm | image NAME",
        "Show what a microVM runs, or an image's documents",
    ),
    ("inspect", "list", "vm | image", "List microVMs, or images"),
    (
        "inspect",
        "logs",
        "vm NAME",
        "Show what a microVM's command printed",
    ),
    (
        "inspect",
        "stats",
        "vm [NAME]",
        "Watch what microVMs take of the host, live",
    ),
    (
        "inspect",
        "top",
        "vm NAME [ps OPTIONS]",
        "Show a microVM's processes, as ps lays them out",
    ),
    (
        "manage",
        "configure",
        "guest --kernel FILE --init FILE",
        "Choose the kernel and shards-init runs boot",
    ),
    ("manage", "kill", "vm NAME", "Kill microVMs"),
    (
        "manage",
        "pause",
        "vm NAME",
        "Freeze microVMs where they are: every vCPU and device, at no CPU",
    ),
    (
        "manage",
        "prune",
        "vm | image | system",
        "Remove stopped microVMs, unused images, or both",
    ),
    (
        "manage",
        "remove",
        "vm | image NAME",
        "Remove microVMs, or images with their stopped microVMs",
    ),
    ("manage", "rename", "vm NAME NEW_NAME", "Name a microVM again"),
    (
        "manage",
        "stop",
        "daemon",
        "End the daemon's runs, and the daemon",
    ),
    (
        "manage",
        "stop",
        "vm NAME",
        "Stop microVMs: their stop signal, then SIGKILL once their grace is up",
    ),
    ("manage", "tag", "image SOURCE TARGET", "Name an image again"),
    (
        "manage",
        "unpause",
        "vm NAME",
        "Let frozen microVMs run on from where they were",
    ),
    (
        "images",
        "build",
        "image PATH",
        "Build an image from a Dockerfile or Agentfile, and make it a microVM",
    ),
    (
        "images",
        "export",
        "vm NAME",
        "Write a microVM's files out as a tar archive",
    ),
    (
        "images",
        "load",
        "image",
        "Load images from a tar archive or stdin",
    ),
    (
        "images",
        "pull",
        "image NAME",
        "Download an image, and make it a microVM",
    ),
    ("images", "push", "image NAME", "Upload an image to a registry"),
    ("images", "save", "image NAME", "Save images to a tar archive"),
];

/// Whether `word` is an action of shards' grammar.
pub fn is_action(word: &str) -> bool {
    ACTIONS.iter().any(|(_, a, _, _)| *a == word)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(words: &str) -> Option<String> {
        let args: Vec<String> = words.split(' ').map(String::from).collect();
        rewrite(&args).map(|w| w.join(" "))
    }

    #[test]
    fn actions_on_things_are_said_as_shards_runs_them() {
        assert_eq!(said("stop vm web db").as_deref(), Some("stop web db"));
        assert_eq!(said("stop container web").as_deref(), Some("stop web"));
        assert_eq!(said("remove vm -f web").as_deref(), Some("rm -f web"));
        assert_eq!(
            said("remove image alpine:3.22").as_deref(),
            Some("rmi --vms alpine:3.22")
        );
        assert_eq!(said("list vm -a").as_deref(), Some("ps -a"));
        assert_eq!(said("ls images").as_deref(), Some("images"));
        assert_eq!(
            said("run vm -d alpine sleep 9").as_deref(),
            Some("run -d alpine sleep 9")
        );
        assert_eq!(
            said("inspect image alpine").as_deref(),
            Some("image inspect alpine")
        );
        assert_eq!(said("pull image alpine").as_deref(), Some("pull alpine"));
        assert_eq!(
            said("run vm --kernel k --cpus 2").as_deref(),
            Some("run --kernel k --cpus 2")
        );
        assert_eq!(said("restore vm dir").as_deref(), Some("restore dir"));
        // Docker's management form, turned round.
        assert_eq!(said("image rm alpine").as_deref(), Some("rmi --vms alpine"));
        assert_eq!(said("container ls").as_deref(), Some("ps"));
        // `shards vm ...` is no form of shards'.
        assert_eq!(said("vm ls"), None);
        assert_eq!(
            said("run daemon --detached").as_deref(),
            Some("daemon --detached")
        );
        assert_eq!(said("stop daemon").as_deref(), Some("daemon stop"));
        assert_eq!(
            said("configure guest --kernel k --init i").as_deref(),
            Some("guest use --kernel k --init i")
        );
        assert_eq!(said("inspect guest").as_deref(), Some("guest"));
    }

    #[test]
    fn docker_words_and_the_vm_process_are_left_alone() {
        for words in [
            "stop web",
            "rm -f web",
            "rmi alpine",
            "ps -a",
            "images",
            "run alpine",
            "restore dir",
            "pull alpine",
        ] {
            assert_eq!(said(words), None, "{words}");
        }
    }
}

#[cfg(test)]
mod sorted {
    use super::*;

    #[test]
    fn each_section_lists_its_actions_in_order_and_names_every_action_once() {
        for section in SECTIONS {
            let rows: Vec<(&str, &str)> = ACTIONS
                .iter()
                .filter(|r| r.0 == *section)
                .map(|r| (r.1, r.2))
                .collect();
            let mut sorted = rows.clone();
            sorted.sort();
            assert_eq!(rows, sorted, "{section}");
        }
        assert!(ACTIONS.iter().all(|r| SECTIONS.contains(&r.0)));
        // An action a line, and each of its ways said on its page.
        let mut seen = std::collections::HashSet::new();
        assert!(ACTIONS.iter().all(|r| seen.insert(r.1)));
        assert!(USES.iter().all(|u| ACTIONS.iter().any(|a| a.1 == u.1)));
        assert!(ACTIONS.iter().all(|a| USES.iter().any(|u| u.1 == a.1)));
    }
}
