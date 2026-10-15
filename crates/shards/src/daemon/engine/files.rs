//! A microVM's files and processes through the API (D170): `shards copy vm` in and out
//! (the archive calls), `shards export vm`, `shards diff vm` and `shards top vm`.
//!
//! Archives are packed and unpacked in the microVM's own kernel, by its init: a hostile
//! archive, or a path swapped as it is copied, stays behind the hypervisor. What a
//! microVM's guest packs goes from it straight onto the client's connection, never
//! through the daemon; what a client uploads is taken as it comes, never staged.

use std::io::BufRead;
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;

use base64::Engine as _;
use serde_json::{Value, json};

use super::http::{self, Answer};
use super::{Call, Done, refused};
use crate::build::proxy::head::Body;
use crate::containers::Disk;
use crate::daemon::{COPY_STEP, Daemon, Threads, lock};

/// The tar archives the API carries.
const TAR: &str = "application/x-tar";

/// The field a path's stat goes in: its JSON (name, size, mode, mtime, linkTarget) in
/// URL-safe base64, as the API's clients read it.
const PATH_STAT: &str = "X-Docker-Container-Path-Stat";

impl<D: Disk> Daemon<D> {
    /// `HEAD` and `GET /containers/{id}/archive?path=`: `shards copy vm` out of it. The
    /// path's stat, as its guest has it; for a GET, its tar archive too, packed by the
    /// microVM's init and written by it onto the connection.
    pub(super) fn engine_archive<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        given: &str,
        call: &Call<'_>,
    ) -> Done {
        let Some(path) = call.query("path").filter(|p| !p.is_empty()) else {
            return Done::Answer(Answer::error(400, "a copy names its path in the microVM (path)"));
        };
        let stat = match self.ask_command(threads, &[COPY_STEP, "stat", given, path]) {
            Ok((0, out, _)) => base64::engine::general_purpose::URL_SAFE.encode(out.trim_ascii_end()),
            Ok((_, _, err)) => return Done::Answer(refused(&String::from_utf8_lossy(&err), 500)),
            Err(e) => return Done::Answer(Answer::error(500, e)),
        };
        if call.head.method == "HEAD" {
            let mut answer = Answer::text(200, "");
            answer.fields.push((PATH_STAT, stat));
            return Done::Answer(answer);
        }
        if http::head_to_close(conn, 200, TAR, &[(PATH_STAT, stat)]).is_err() {
            return Done::Close;
        }
        // Its archive: once the head is out, nothing more can be said but the archive.
        let _ = self.ask_command_with(threads, &[COPY_STEP, "archive", given, path], &[conn.as_fd()]);
        shards_ipc::end(conn);
        Done::Close
    }

    /// `PUT /containers/{id}/archive?path=`: `shards copy vm` into it. The request's tar
    /// archive, taken as it comes, unpacked by the microVM's init into directory `path`;
    /// its owners the microVM's user with `copyUIDGID`, and a directory never replaced by
    /// what is not one, nor the other way round, with `noOverwriteDirNonDir`.
    pub(super) fn engine_extract<'s, 'e, R: BufRead>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        given: &str,
        call: &Call<'_>,
        body: &mut Body<'_, R>,
    ) -> Done {
        let Some(path) = call.query("path").filter(|p| !p.is_empty()) else {
            return Done::Answer(Answer::error(
                400,
                "a copy names its directory in the microVM (path)",
            ));
        };
        let owners = if call.flag("copyUIDGID") { "1" } else { "0" };
        let overwrite = if call.flag("noOverwriteDirNonDir") {
            "0"
        } else {
            "1"
        };
        let answered = self.ask_with_body(
            threads,
            &[COPY_STEP, "extract", given, path, owners, overwrite],
            body,
        );
        Done::Answer(match answered {
            Ok((0, _, _)) => Answer::text(200, ""),
            Ok((_, _, said)) => refused(&String::from_utf8_lossy(&said), 500),
            Err(e) => Answer::error(500, e),
        })
    }

    /// `GET /containers/{id}/export`: `shards export vm`. Its files as a tar archive, which
    /// its init writes onto the connection.
    pub(super) fn engine_export<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        given: &str,
    ) -> Done {
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => return Done::Answer(refused(&e, 500)),
        };
        // Said before the head, which nothing can be said after.
        if lock(&self.paused).contains(&id) {
            return Done::Answer(Answer::error(
                409,
                format!(
                    "{given} is paused: its files are in its frozen guest, and can be exported once it is unpaused"
                ),
            ));
        }
        if http::head_to_close(conn, 200, TAR, &[]).is_err() {
            return Done::Close;
        }
        let _ = self.ask_command_with(threads, &["export", &id], &[conn.as_fd()]);
        shards_ipc::end(conn);
        Done::Close
    }

    /// `GET /containers/{id}/changes`: `shards diff vm`. What it changed of its image's
    /// files, as its guest keeps its writable layer: each path, and how (0 changed, 1
    /// added, 2 deleted).
    pub(super) fn engine_changes<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, given: &str) -> Answer {
        match self.ask_command(threads, &["diff", given]) {
            Ok((0, out, _)) => {
                let changes: Vec<Value> = String::from_utf8_lossy(&out)
                    .lines()
                    .filter_map(|line| {
                        let (how, path) = line.split_once(' ')?;
                        let kind = match how {
                            "C" => 0,
                            "A" => 1,
                            "D" => 2,
                            _ => return None,
                        };
                        Some(json!({ "Path": path, "Kind": kind }))
                    })
                    .collect();
                Answer::json(200, &Value::Array(changes))
            }
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 500),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `GET /containers/{id}/top?ps_args=`: `shards top vm`. Its processes as its guest has
    /// them, its own PIDs, its users named as its own `/etc/passwd` names them, laid out as
    /// `ps` lays them out with the options given (`-ef` if none), no `ps` in its image.
    pub(super) fn engine_top(&self, given: &str, call: &Call<'_>) -> Answer {
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => return refused(&e, 500),
        };
        let options = call.query("ps_args").filter(|a| !a.is_empty()).unwrap_or("-ef");
        match self.top_table(given, &id, options, crate::cli::utc_offset()) {
            Ok(t) => Answer::json(200, &json!({ "Titles": t.titles, "Processes": t.processes })),
            Err(e) => refused(&e, 500),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copy's refusals keep their words, without the CLI's prefixes, and their status:
    /// a path that is not there, a microVM whose files take no writes.
    #[test]
    fn a_copys_refusals_are_said_with_their_status() {
        let missing = refused("shards: Could not find the file /x in container c\n", 500);
        assert_eq!(missing.status, 404);
        assert!(
            String::from_utf8_lossy(&missing.body).contains(r#""Could not find the file /x in container c""#)
        );
        assert_eq!(
            refused("shards: container rootfs is marked read-only\n", 500).status,
            403
        );
    }
}
