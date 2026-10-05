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
}

impl Thing {
    fn of(word: &str) -> Option<Thing> {
        match word {
            "vm" | "vms" | "microvm" | "microvms" => Some(Thing::Vm),
            "container" | "containers" => Some(Thing::Container),
            "image" | "images" => Some(Thing::Image),
            "daemon" => Some(Thing::Daemon),
            "guest" => Some(Thing::Guest),
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
        (Some(Thing::Vm | Thing::Daemon | Thing::Guest), _) => return None,
        _ => (first, Thing::of(second?)?, args.get(2..)?),
    };
    let said: &[&str] = match (action, thing) {
        ("list" | "ls" | "ps", Thing::Vm | Thing::Container) => &["ps"],
        ("list" | "ls", Thing::Image) => &["images"],
        ("remove" | "rm" | "delete", Thing::Vm | Thing::Container) => &["rm"],
        // An image goes with the stopped microVMs made from it.
        ("remove" | "rm" | "delete" | "rmi", Thing::Image) => &["rmi", "--vms"],
        ("inspect", Thing::Image) => &["image", "inspect"],
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

/// The actions shards' grammar has, for its help: each with the things it takes, and
/// what it does.
pub static ACTIONS: &[(&str, &str, &str)] = &[
    (
        "run",
        "vm IMAGE",
        "Run a command in a new microVM, made from the image if it is a container's",
    ),
    (
        "run",
        "vm --kernel FILE",
        "Boot a kernel directly in a microVM of its own",
    ),
    ("list", "vm | image", "List microVMs, or images"),
    (
        "stop",
        "vm NAME",
        "Stop microVMs: their stop signal, then SIGKILL once their grace is up",
    ),
    ("kill", "vm NAME", "Kill microVMs"),
    ("remove", "vm | image NAME", "Remove microVMs, or images"),
    ("logs", "vm NAME", "Show what a microVM's command printed"),
    ("exec", "vm NAME", "Run a command in a running microVM"),
    ("pull", "image NAME", "Download an image, and make it a microVM"),
    ("push", "image NAME", "Upload an image to a registry"),
    ("inspect", "image NAME", "Show an image's documents"),
    (
        "inspect",
        "vm NAME",
        "Show a microVM's document: what it runs, how it stands, its microVM",
    ),
    ("restore", "vm DIR", "Resume a microVM from a snapshot"),
    (
        "run",
        "daemon",
        "Serve runs from warm microVMs (the first run starts it)",
    ),
    ("stop", "daemon", "End the daemon's runs, and the daemon"),
    (
        "configure",
        "guest --kernel FILE --init FILE",
        "Choose the kernel and shards-init runs boot",
    ),
    ("inspect", "guest", "Show the guest runs boot"),
];

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
