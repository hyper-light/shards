//! PM M535's measurement: a read waiting on an AF_UNIX stream while its peer writes three
//! bytes, ends its side and closes, as `client::borrowed_tests::exact_bytes_clone_and_half_close`
//! does, again and again (`SHARDS_AFUNIX_ROUNDS`, 2000 by default) for each way the peer
//! ends (shutdown(SD_SEND) through a copy of its socket, as that test's clone does; through
//! the socket itself; or a close alone), with the peer's socket blocking or not (shards-api
//! sets its own not to), and each way the reader waits: in recv itself, or in WSAPoll
//! first. Prints how many reads saw no end within 2 s, each then ended from this side
//! (shutdown(SD_BOTH) through a copy of the reader's socket) so its thread goes.
use crate::UnixStream;
use std::io::{Read as _, Write as _};
use std::net::Shutdown;
use std::sync::mpsc;
use std::time::Duration;

#[test]
#[ignore = "a measurement, run by hand (PM M535)"]
fn a_waiting_reads_end_beside_its_peers_half_close() {
    let rounds: usize = std::env::var("SHARDS_AFUNIX_ROUNDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(2000);
    for (ends, waiting, polled) in ["half-close through a copy", "half-close", "close"]
        .into_iter()
        .flat_map(|ends| [true, false].map(move |waiting| (ends, waiting)))
        .flat_map(|(ends, waiting)| [false, true].map(move |polled| (ends, waiting, polled)))
    {
        let (mut lost, mut freed) = (0, 0);
        for _ in 0..rounds {
            let (client, server) = UnixStream::pair().unwrap();
            let holder = server.try_clone().unwrap();
            let (done, ended) = mpsc::channel();
            std::thread::spawn(move || {
                let mut got = [0u8; 3];
                (&server).read_exact(&mut got).unwrap();
                if polled {
                    assert!(server.ready_within(false, -1).unwrap());
                }
                let _ = done.send((&server).read(&mut [0u8; 1]).map_err(|e| e.kind()));
            });
            let copy = client.try_clone().unwrap();
            client.set_nonblocking(!waiting).unwrap();
            let mut writer = if ends == "half-close through a copy" {
                &copy
            } else {
                &client
            };
            writer.write_all(&[0xff, 0, b'\n']).unwrap();
            if ends != "close" {
                writer.shutdown(Shutdown::Write).unwrap();
            }
            drop(copy);
            drop(client);
            match ended.recv_timeout(Duration::from_secs(2)) {
                Ok(Ok(0)) => {}
                Ok(other) => panic!("{ends}, polled {polled}: {other:?}"),
                Err(_) => {
                    lost += 1;
                    let _ = holder.shutdown(Shutdown::Both);
                    if ended.recv_timeout(Duration::from_secs(2)).is_ok() {
                        freed += 1;
                    }
                }
            }
        }
        println!(
            "ends={ends:?} peer={} reader={} rounds={rounds} lost={lost} freed_by_own_shutdown={freed}",
            if waiting { "blocking" } else { "not blocking" },
            if polled { "WSAPoll, then recv" } else { "recv" }
        );
    }
}
