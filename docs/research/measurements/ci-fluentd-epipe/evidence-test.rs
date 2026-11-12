
    /// Evidence for ci-fluentd-epipe (run on the stream before its fix, not kept in the
    /// crate): to the receiver of `a_write_that_meets_its_receivers_end_is_followed_as_one_seen`,
    /// drive a stream as a shipper does, a short message sent, then the long one given again
    /// until it is sent (`until_done`, the driver's retries) and the close asked again until
    /// it ends, 40 tries in all, 10 ms apart; say what each try said, whether the long one was
    /// ever held, and whether it was ever pointed at.
    #[test]
    #[ignore = "evidence: the stream before its fix"]
    fn evidence_a_write_meeting_its_receivers_end() {
        let (server, client) = tls();
        let (_stop, stopped) = shards_ipc::Stream::pair().unwrap();
        let wait = Wait(&stopped);
        let long = [vec![b'L'; 16 << 20], vec![0]].concat();
        for via in ["tcp", "tls", "unix"] {
            let dir = shards_testdir::TempDir::new("we").unwrap();
            let (got, _got_frames) = std::sync::mpsc::channel::<Vec<u8>>();
            let to = if via == "unix" {
                let path = dir.join("r.sock");
                let listener = shards_vmm::platform::UnixListener::bind(&path).unwrap();
                std::thread::spawn(move || {
                    while let Ok((conn, _)) = listener.accept() {
                        if !ends_past_100(conn, &got, |c, buf| shards_ipc::read(&*c, buf)) {
                            return;
                        }
                    }
                });
                To::Unix(path)
            } else {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let port = listener.local_addr().unwrap().port();
                let tls = (via == "tls").then(|| server.clone());
                let to = if tls.is_some() {
                    To::Tls(format!("localhost:{port}"), client.clone())
                } else {
                    To::Tcp(format!("127.0.0.1:{port}"))
                };
                std::thread::spawn(move || {
                    for conn in listener.incoming() {
                        let Ok(conn) = conn else { return };
                        let going = match &tls {
                            None => ends_past_100(conn, &got, |c, buf| shards_ipc::read(&*c, buf)),
                            Some(server) => {
                                let tls = rustls::StreamOwned::new(
                                    rustls::ServerConnection::new(server.clone()).unwrap(),
                                    conn,
                                );
                                ends_past_100(tls, &got, |t, buf| t.read(buf))
                            }
                        };
                        if !going {
                            return;
                        }
                    }
                });
                to
            };
            let mut stream = Stream::new(to, [None; 2]);
            stream.send(b"first\0", &wait).unwrap();
            let (mut sent, mut ever_held, mut pointed) = (false, false, false);
            let mut said: Vec<String> = Vec::new();
            for _ in 0..40 {
                if stream.suspect(10).is_some() {
                    pointed = true;
                    break;
                }
                let r = if sent {
                    stream.close(&wait)
                } else {
                    stream.send(&long, &wait).map(|()| sent = true)
                };
                ever_held |= stream.held.iter().any(|h| h.message.len() == long.len());
                said.push(match r {
                    Ok(()) => "ok".into(),
                    Err(e) => format!("{:?}", e.kind()),
                });
                if sent && said.last().is_some_and(|s| s == "ok") && stream.held() == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
            for s in &said {
                *counts.entry(s.as_str()).or_default() += 1;
            }
            eprintln!(
                "EVIDENCE {via}: tries {} said {counts:?}, long sent {sent}, ever held {ever_held}, \
                 pointed at {pointed}",
                said.len()
            );
        }
    }
