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
    /// Volumes: directories microVMs mount, which outlive them.
    Volume,
    /// Agents, harnesses and MCP servers: OSI artifacts (D54), shards' own groups.
    Agent,
    Harness,
    Mcp,
    /// An Agentfile made from an image (§10).
    Agentfile,
    /// A registry images are pulled from and pushed to, which `login` and `logout` reach.
    Registry,
    /// A microVM's snapshots, what `save vm` saves it as (D176).
    Snapshot,
    /// Another machine's shards, which this one's commands run on whole (D180).
    Context,
    /// A machine this one's daemon enrolled, which runs shards here (D180).
    Client,
    /// What enrolls a client, once (D180).
    Invitation,
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
            "volume" | "volumes" => Some(Thing::Volume),
            "agent" | "agents" => Some(Thing::Agent),
            "harness" | "harnesses" => Some(Thing::Harness),
            "mcp" => Some(Thing::Mcp),
            "agentfile" | "agentfiles" => Some(Thing::Agentfile),
            "registry" | "registries" => Some(Thing::Registry),
            "snapshot" | "snapshots" => Some(Thing::Snapshot),
            "context" | "contexts" => Some(Thing::Context),
            "client" | "clients" => Some(Thing::Client),
            "invitation" | "invitations" => Some(Thing::Invitation),
            _ => None,
        }
    }
}

/// `args` said as the command shards runs, if they are in its grammar; `None` if they
/// are not, and are read as they are.
pub fn rewrite(args: &[String]) -> Option<Vec<String>> {
    let (first, second) = (args.first()?.as_str(), args.get(1).map(String::as_str));
    // What a microVM or an image holds, read once or watched as it comes (D123): what a
    // microVM's command printed, what it changed of its image's files, how an image's
    // layers were made. `read ports`, `read processes` and `watch processes` are commands
    // of their own (commands.rs).
    let view: Option<&[&str]> = match (first, second) {
        ("read", Some("logs")) => Some(&["logs"]),
        ("watch", Some("logs")) => Some(&["logs", "--follow"]),
        ("read", Some("changes")) => Some(&["diff"]),
        ("read", Some("history")) => Some(&["history", "--convert"]),
        _ => None,
    };
    if let Some(said) = view {
        let mut out: Vec<String> = said.iter().map(|w| (*w).to_string()).collect();
        out.extend(args.get(2..)?.iter().cloned());
        return Some(out);
    }
    // `shards image ls` is `shards ls image`: the THING ACTION form, turned round.
    let thing_first = matches!(Thing::of(first), Some(Thing::Image | Thing::Container));
    let (action, thing, rest) = match (Thing::of(first), second) {
        (Some(thing @ (Thing::Image | Thing::Container)), Some(action)) => (action, thing, args.get(2..)?),
        (
            Some(
                Thing::Vm
                | Thing::Daemon
                | Thing::Guest
                | Thing::Disk
                | Thing::System
                | Thing::Volume
                | Thing::Agent
                | Thing::Harness
                | Thing::Mcp
                | Thing::Agentfile
                | Thing::Registry
                | Thing::Snapshot
                | Thing::Context
                | Thing::Client
                | Thing::Invitation,
            ),
            _,
        ) => {
            return None;
        }
        _ => (first, Thing::of(second?)?, args.get(2..)?),
    };
    let said: &[&str] = match (action, thing) {
        ("list" | "ls" | "ps", Thing::Vm | Thing::Container) => &["ps"],
        ("list" | "ls", Thing::Image) => &["images"],
        ("remove" | "rm" | "delete", Thing::Vm | Thing::Container) => &["rm"],
        // An image goes with the stopped microVMs made from it.
        ("remove" | "rm" | "delete" | "rmi", Thing::Image) => &["rmi", "--vms"],
        // An image not yet a microVM is made one first, as anything but its removal
        // asked of it does (`--convert`); Docker's own order, `image inspect`, answers
        // as Docker's does.
        ("inspect", Thing::Image) => &["image", "inspect", "--convert"],
        ("inspect", Thing::Disk) => &["system", "df"],
        ("prune", Thing::Vm | Thing::Container) => &["container", "prune"],
        ("prune", Thing::Image) => &["image", "prune"],
        ("prune", Thing::System) => &["system", "prune"],
        ("list" | "ls", Thing::Volume) => &["volume", "ls"],
        ("list" | "ls", Thing::Snapshot) => &["snapshot", "ls"],
        ("remove" | "rm" | "delete", Thing::Snapshot) => &["snapshot", "rm"],
        ("create" | "make", Thing::Context) => &["context", "create"],
        ("use" | "set", Thing::Context) => &["context", "use"],
        ("list" | "ls", Thing::Context) => &["context", "ls"],
        ("inspect" | "show", Thing::Context) => &["context", "inspect"],
        ("remove" | "rm" | "delete", Thing::Context) => &["context", "rm"],
        ("invite", Thing::Client) => &["client", "invite"],
        ("list" | "ls", Thing::Client) => &["client", "ls"],
        ("remove" | "rm" | "delete", Thing::Client) => &["client", "rm"],
        ("list" | "ls", Thing::Invitation) => &["invitation", "ls"],
        ("remove" | "rm" | "delete", Thing::Invitation) => &["invitation", "rm"],
        ("create" | "make", Thing::Volume) => &["volume", "create"],
        ("create" | "make", Thing::Agentfile) => &["agentfile"],
        ("remove" | "rm" | "delete", Thing::Volume) => &["volume", "rm"],
        ("inspect", Thing::Volume) => &["volume", "inspect"],
        ("prune", Thing::Volume) => &["volume", "prune"],
        ("rename", Thing::Vm | Thing::Container) => &["rename"],
        ("watch", Thing::Vm | Thing::Container) => &["stats"],
        ("pause" | "freeze", Thing::Vm | Thing::Container) => &["pause"],
        ("watch", Thing::System) => &["events"],
        ("inspect", Thing::System) => &["info"],
        ("export", Thing::Vm | Thing::Container) => &["export"],
        ("start", Thing::Vm | Thing::Container) => &["start"],
        ("commit", Thing::Vm | Thing::Container) => &["commit"],
        ("copy" | "cp", Thing::Vm | Thing::Container) => &["cp"],
        ("restart", Thing::Vm | Thing::Container) => &["restart"],
        ("create" | "make", Thing::Vm | Thing::Container) => &["create"],
        ("unpause" | "resume" | "thaw", Thing::Vm | Thing::Container) => &["unpause"],
        ("inspect", Thing::Vm | Thing::Container) => &["container", "inspect", "--convert"],
        ("run", Thing::Vm | Thing::Container) => &["run"],
        ("restore", Thing::Vm) => &["restore"],
        ("run" | "start", Thing::Daemon) => &["daemon"],
        ("stop", Thing::Daemon) => &["daemon", "stop"],
        ("configure" | "use" | "set", Thing::Guest) => &["guest", "use"],
        ("inspect" | "show", Thing::Guest) => &["guest"],
        ("login", Thing::Registry) => &["login"],
        ("logout", Thing::Registry) => &["logout"],
        ("search", Thing::Image) => &["search"],
        (action @ ("stop" | "kill" | "wait" | "exec"), Thing::Vm | Thing::Container) => {
            return Some(once(action, rest));
        }
        // An agent, harness or MCP server: shards' own group, `shards agent build ...`.
        (
            action @ ("build" | "push" | "pull" | "list" | "ls" | "inspect" | "remove" | "rm" | "delete"
            | "sign"),
            thing @ (Thing::Agent | Thing::Harness | Thing::Mcp),
        ) => {
            let group = match thing {
                Thing::Agent => "agent",
                Thing::Harness => "harness",
                _ => "mcp",
            };
            let mut out = vec![group.to_string(), action.to_string()];
            out.extend(rest.iter().cloned());
            return Some(out);
        }
        // A running microVM saved whole, to restore later (D176): shards' own, as Docker's
        // `checkpoint create` is its own command.
        ("save", Thing::Vm | Thing::Container) => &["save", "vm"],
        (action @ ("push" | "tag" | "save"), _) if !thing_first => {
            let mut out = once(action, &[]);
            out.push("--convert".into());
            out.extend(rest.iter().cloned());
            return Some(out);
        }
        (action @ ("pull" | "load" | "build"), _) => {
            return Some(once(action, rest));
        }
        _ => return None,
    };
    let mut out: Vec<String> = said
        .iter()
        .filter(|w| !(thing_first && **w == "--convert"))
        .map(|w| (*w).to_string())
        .collect();
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
    (
        "run",
        "create",
        "vm | volume | agentfile | context",
        "Make a microVM without starting it, a volume, an Agentfile of an image, or a context",
    ),
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
        "save",
        "vm | image",
        "Save a running microVM whole, to restore later, or images to a tar archive",
    ),
    (
        "run",
        "start",
        "vm",
        "Start stopped microVMs again, their files as they left them",
    ),
    (
        "inspect",
        "inspect",
        "vm | image | volume | disk | guest | system | context",
        "Show a microVM, an image, a volume, disk use, the guest, shards itself, or a context",
    ),
    (
        "inspect",
        "list",
        "vm | image | volume | snapshot | context | client | invitation",
        "List microVMs, images, volumes, a microVM's snapshots, contexts, or this machine's clients and invitations",
    ),
    (
        "inspect",
        "read",
        "processes | ports | logs | changes | history",
        "Read what a microVM ran, where its ports go, what its command printed or changed, or how an image was made",
    ),
    (
        "inspect",
        "watch",
        "vms | processes | logs | system",
        "Watch microVMs as they run: what they take of the host, their processes, what a command prints, what happens to them",
    ),
    (
        "manage",
        "configure",
        "guest",
        "Choose the kernel and shards-init runs boot",
    ),
    ("manage", "copy", "vm", "Copy files into or out of a microVM"),
    (
        "manage",
        "invite",
        "client",
        "Let another machine run shards here, by an invitation it joins with",
    ),
    ("manage", "kill", "vm", "Kill microVMs"),
    ("manage", "pause", "vm", "Freeze microVMs where they are"),
    (
        "manage",
        "prune",
        "vm | image | volume | system",
        "Remove stopped microVMs, unused images or volumes, or all of them",
    ),
    (
        "manage",
        "remove",
        "vm | image | volume | snapshot | context | client | invitation",
        "Remove microVMs, images with their stopped microVMs, volumes, snapshots, contexts, clients, or invitations",
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
        "manage",
        "use",
        "context",
        "Choose the machine shards' commands run on",
    ),
    (
        "images",
        "build",
        "image",
        "Build an image from a Dockerfile or Agentfile, and make it a microVM",
    ),
    (
        "images",
        "commit",
        "vm",
        "Make an image of what a microVM changed",
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
    ("images", "login", "registry", "Log in to a registry"),
    ("images", "logout", "registry", "Forget a registry's credentials"),
    (
        "images",
        "pull",
        "image",
        "Download an image, and make it a microVM",
    ),
    ("images", "push", "image", "Upload an image to a registry"),
    (
        "images",
        "search",
        "image",
        "Find images in registries, and whether each runs here",
    ),
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
    (
        "run",
        "create",
        "volume [NAME]",
        "Make a volume microVMs can mount, named or not",
    ),
    (
        "run",
        "create",
        "context NAME --invite-file FILE",
        "Join another machine's shards by its owner's invitation, as context NAME",
    ),
    ("run", "restore", "vm DIR", "Resume a microVM from a snapshot"),
    (
        "run",
        "restore",
        "vm NAME [SNAPSHOT]",
        "Resume a stopped microVM where it was saved, from its newest snapshot unless SNAPSHOT names one",
    ),
    (
        "run",
        "save",
        "vm NAME [SNAPSHOT]",
        "Save a running microVM whole, its memory, processes and devices as they are; then stop it, or leave it running",
    ),
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
    (
        "inspect",
        "inspect",
        "volume NAME",
        "Show a volume: where its files are, and its options",
    ),
    (
        "inspect",
        "inspect",
        "context NAME",
        "Show a context: its machine's address, and the key its daemon presents",
    ),
    (
        "inspect",
        "list",
        "vm | image | volume",
        "List microVMs, images, or volumes",
    ),
    (
        "inspect",
        "list",
        "snapshots NAME",
        "List a microVM's snapshots, newest first: when each was saved, and what it takes on disk",
    ),
    (
        "inspect",
        "list",
        "contexts",
        "List the machines this one's commands may run on, the one they use marked",
    ),
    (
        "inspect",
        "list",
        "clients | invitations",
        "List the machines enrolled to run shards here, or the invitations still open",
    ),
    (
        "inspect",
        "read",
        "processes NAME",
        "The processes a microVM's members started and ended, each whole, as the kernel saw them",
    ),
    (
        "inspect",
        "read",
        "ports NAME",
        "Which of a microVM's members each published port reaches",
    ),
    ("inspect", "read", "logs NAME", "What a microVM's command printed"),
    (
        "inspect",
        "read",
        "changes NAME",
        "What a microVM changed of its image's files",
    ),
    (
        "inspect",
        "read",
        "history IMAGE",
        "How an image's layers were made",
    ),
    (
        "inspect",
        "watch",
        "vms [NAME...]",
        "What microVMs take of the host, live",
    ),
    (
        "inspect",
        "watch",
        "processes NAME",
        "A microVM's processes as they start and end",
    ),
    (
        "inspect",
        "watch",
        "logs NAME",
        "What a microVM's command prints, as it prints it",
    ),
    (
        "inspect",
        "watch",
        "system",
        "What happens to microVMs and images, as it happens",
    ),
    (
        "manage",
        "configure",
        "guest --kernel FILE --init FILE",
        "Choose the kernel and shards-init runs boot",
    ),
    (
        "manage",
        "copy",
        "vm NAME:PATH PATH | vm PATH NAME:PATH",
        "Copy files out of a microVM, or into one",
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
        "prune",
        "volume",
        "Remove volumes no microVM mounts: anonymous ones, or all with --all",
    ),
    (
        "manage",
        "remove",
        "vm | image NAME",
        "Remove microVMs, or images with their stopped microVMs",
    ),
    (
        "manage",
        "remove",
        "volume NAME",
        "Remove volumes no microVM mounts",
    ),
    (
        "manage",
        "remove",
        "snapshot NAME SNAPSHOT...",
        "Remove snapshots of a microVM",
    ),
    (
        "manage",
        "remove",
        "context | client | invitation NAME",
        "Forget a context, or end a client's enrollment here, or an invitation's",
    ),
    (
        "manage",
        "invite",
        "client --name NAME --output FILE",
        "Let another machine run shards here: an invitation, a private file that enrolls it once",
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
        "manage",
        "use",
        "context NAME",
        "Run shards' commands on context NAME's machine from now on; `default` is this one",
    ),
    (
        "images",
        "build",
        "image PATH",
        "Build an image from a Dockerfile or Agentfile, and make it a microVM",
    ),
    (
        "images",
        "commit",
        "vm NAME [IMAGE]",
        "Make an image of what a microVM changed",
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
        "login",
        "registry [SERVER]",
        "Log in to a registry, Docker Hub's unless SERVER names another",
    ),
    (
        "images",
        "logout",
        "registry [SERVER]",
        "Forget the credentials kept for a registry",
    ),
    (
        "images",
        "pull",
        "image NAME",
        "Download an image, and make it a microVM",
    ),
    ("images", "push", "image NAME", "Upload an image to a registry"),
    ("images", "save", "image NAME", "Save images to a tar archive"),
    (
        "images",
        "search",
        "image TERM",
        "Find images on Docker Hub, or in the registry TERM names: whether each runs on this machine, and what its pull takes",
    ),
];

/// shards' page of its grammar, plain: its usage and `about`, then each section's actions,
/// what each acts on and what it does, as its page on a colour terminal lists them.
pub fn help(about: &str) -> String {
    let width = ACTIONS.iter().map(|a| a.1.len()).max().unwrap_or(0);
    let mut text = format!("Usage:  shards ACTION THING [ARG...]\n\n{about}\n");
    for section in SECTIONS {
        let mut heading = section.chars();
        let heading: String = heading
            .next()
            .map(|c| c.to_ascii_uppercase())
            .into_iter()
            .chain(heading)
            .collect();
        text.push_str(&format!("\n{heading}:\n"));
        for (_, action, things, about) in ACTIONS.iter().filter(|a| a.0 == *section) {
            text.push_str(&format!(
                "  {action:<width$}  {things}\n  {:width$}  {about}\n",
                ""
            ));
        }
    }
    text.push_str("\nRun 'shards ACTION' for each way an action is said.\n");
    text
}

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
        // Anything but removal makes an image a microVM first; Docker's order does not.
        assert_eq!(
            said("inspect image alpine").as_deref(),
            Some("image inspect --convert alpine")
        );
        assert_eq!(
            said("image inspect alpine").as_deref(),
            Some("image inspect alpine")
        );
        assert_eq!(
            said("inspect vm ubuntu").as_deref(),
            Some("container inspect --convert ubuntu")
        );
        // Read once, or watched as it comes (D123).
        assert_eq!(
            said("read history ubuntu").as_deref(),
            Some("history --convert ubuntu")
        );
        assert_eq!(said("read logs web").as_deref(), Some("logs web"));
        assert_eq!(said("watch logs -t web").as_deref(), Some("logs --follow -t web"));
        assert_eq!(said("read changes web").as_deref(), Some("diff web"));
        assert_eq!(said("watch vms").as_deref(), Some("stats"));
        assert_eq!(said("watch vm web").as_deref(), Some("stats web"));
        assert_eq!(said("watch system").as_deref(), Some("events"));
        // Docker's verbs are no form of shards': Docker's own commands answer them.
        for docker in [
            "top vm web",
            "stats vm",
            "events system",
            "diff vm web",
            "history image ubuntu",
            "logs vm web",
            "port vm web",
        ] {
            assert_eq!(said(docker), None, "{docker}");
        }
        assert_eq!(
            said("tag image ubuntu mine").as_deref(),
            Some("tag --convert ubuntu mine")
        );
        assert_eq!(said("push image mine").as_deref(), Some("push --convert mine"));
        assert_eq!(said("save image a b").as_deref(), Some("save --convert a b"));
        // A microVM's snapshots (D176).
        assert_eq!(said("list snapshots web").as_deref(), Some("snapshot ls web"));
        assert_eq!(
            said("remove snapshot web first second").as_deref(),
            Some("snapshot rm web first second")
        );
        assert_eq!(said("snapshot ls web"), None);
        // A running microVM saved whole (D176), however it is named.
        assert_eq!(said("save vm web").as_deref(), Some("save vm web"));
        assert_eq!(
            said("save microvm web first --leave-running").as_deref(),
            Some("save vm web first --leave-running")
        );
        assert_eq!(said("save container web").as_deref(), Some("save vm web"));
        assert_eq!(said("pull image alpine").as_deref(), Some("pull alpine"));
        assert_eq!(
            said("run vm --kernel k --cpus 2").as_deref(),
            Some("run --kernel k --cpus 2")
        );
        assert_eq!(said("restore vm dir").as_deref(), Some("restore dir"));
        // A saved microVM, by its name and its snapshot's (D176): the daemon's to tell it
        // from a directory holding a snapshot.
        assert_eq!(said("restore vm web first").as_deref(), Some("restore web first"));
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
        assert_eq!(said("list volumes -q").as_deref(), Some("volume ls -q"));
        assert_eq!(said("create volume data").as_deref(), Some("volume create data"));
        assert_eq!(
            said("remove volume -f data").as_deref(),
            Some("volume rm -f data")
        );
        assert_eq!(
            said("inspect volume data").as_deref(),
            Some("volume inspect data")
        );
        assert_eq!(said("prune volumes -a").as_deref(), Some("volume prune -a"));
        // Docker's own `volume` commands are left as they are.
        assert_eq!(said("volume ls"), None);
        // A registry, logged in to and out of (D171).
        assert_eq!(
            said("login registry -u u r.example").as_deref(),
            Some("login -u u r.example")
        );
        assert_eq!(said("logout registry").as_deref(), Some("logout"));
        // Images found in registries (D172).
        assert_eq!(
            said("search image -f is-official=true python").as_deref(),
            Some("search -f is-official=true python")
        );
        assert_eq!(
            said("search images quay.io/prometheus").as_deref(),
            Some("search quay.io/prometheus")
        );
        // OSI artifacts are shards' own groups (D54), signed as cosign signs (D116).
        assert_eq!(said("push agent r/a:1").as_deref(), Some("agent push r/a:1"));
        assert_eq!(
            said("sign mcp r/m:1 --key k").as_deref(),
            Some("mcp sign r/m:1 --key k")
        );
    }

    #[test]
    fn compatible_commands_and_the_vm_process_are_left_alone() {
        for words in [
            "stop web",
            "rm -f web",
            "rmi alpine",
            "ps -a",
            "images",
            "run alpine",
            "restore dir",
            "pull alpine",
            "login r.example",
            "logout",
            "search alpine",
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
