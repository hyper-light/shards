//! `shards pull`, served for the daemon's clients: crate::pull::command, said through
//! the client's connection, its steps sent where the client shows them, cancelled when
//! the client goes or the daemon stops.

use shards_ipc::Progress;

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards pull NAME[:TAG|@DIGEST]`.
    pub(super) fn pull(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &super::commands::Asker,
        reply: &super::commands::Reply<'_>,
    ) -> u8 {
        let progress = |p: &Progress| reply.progress(p);
        let out = crate::pull::Out {
            out: &|line| reply.out(line),
            err: &|line| reply.err(line),
            // On a colour terminal the client shows the pull its own way, from its steps.
            progress: (asker.terminal && asker.color).then_some(&progress),
        };
        let env = |k: &str| shards_ipc::env_value(&asker.registry_env, k);
        let (status, _) = self.cancellable(asker.client, reply.0, |cancel| {
            crate::pull::command(parsed, &self.home, &env, &out, Some(cancel))
        });
        // moby daemon/containerd/image_pull.go: the reference pulled, named without its
        // tag.
        if status == 0
            && let Some(given) = parsed.args.first()
            && let Ok(r) = shards_image::reference::Reference::parse_normalized(given)
        {
            let r = if parsed.bool("all-tags") {
                r
            } else {
                r.tag_name_only()
            };
            self.image_event(&r.familiar(), &r.familiar_name(), "pull");
        }
        status
    }
}
