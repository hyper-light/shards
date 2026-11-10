// Fixture shell for the exact selected Output methods inserted by pure.py.
// Control records requested calls and supplied snapshots; it implements no
// driver, Shared transitions, I/O wake, endpoint lifetime or actual retirement.
// Those remain real remote tests / ROOT-owned caller and VM checks.
use std::fs::File;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());
fn log(message: String) {
    lock(&LOGGED).push(message);
}

struct CallControl {
    snapshot: Mutex<State>,
    stops: AtomicUsize,
    on_finish: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}
impl CallControl {
    fn state(&self) -> State {
        *lock(&self.snapshot)
    }
    fn abort(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
    fn finish(&self) -> io::Result<()> {
        if let Some(callback) = lock(&self.on_finish).as_ref() {
            callback();
        }
        Ok(())
    }
}

struct CallOutput {
    scope: Scope,
    control: CallControl,
    life: Mutex<Life>,
    metadata: Arc<Mutex<File>>,
    credits: Weak<Credits>,
    index: usize,
}
impl CallOutput {
    // ACTUAL OUTPUT METHODS
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod output_calls {
    use super::*;
    use std::fs::OpenOptions;

    fn fixture(
        name: &str,
        life: Life,
        remote: State,
        writable: bool,
    ) -> (Arc<CallOutput>, Arc<Credits>, std::path::PathBuf) {
        let path =
            std::path::PathBuf::from(std::env::var_os("SHARDS_TERMINAL_LIGHT_DIR").unwrap()).join(name);
        let metadata = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        let metadata = if writable {
            metadata
        } else {
            drop(metadata);
            File::open(&path).unwrap()
        };
        let credits = Arc::new(Credits {
            state: Mutex::new(Queue {
                slots: vec![CreditSlot::Waiting],
                next: 0,
                life: QueueLife::Open,
            }),
            changed: Condvar::new(),
        });
        let output = Arc::new(CallOutput {
            scope: Scope {
                run: RunId([7; 32]),
                generation: 1,
                member: 1,
                channel: Channel::Stdout,
            },
            control: CallControl {
                snapshot: Mutex::new(remote),
                stops: AtomicUsize::new(0),
                on_finish: Mutex::new(None),
            },
            life: Mutex::new(life),
            metadata: Arc::new(Mutex::new(metadata)),
            credits: Arc::downgrade(&credits),
            index: 0,
        });
        (output, credits, path)
    }

    fn at(accepted: u64, consumed: u64) -> Progress {
        Progress { accepted, consumed }
    }

    #[test]
    fn finished_end_stop_requests_retirement_and_writes_only_one_complete_row() {
        let (output, credits, path) = fixture(
            "finished-end",
            Life::Draining(Terminal::End),
            State::Finished(at(102, 102)),
            true,
        );
        output.abort();
        output.abort();
        assert_eq!(*lock(&output.life), Life::Ended);
        assert_eq!(
            output.control.stops.load(Ordering::SeqCst),
            2,
            "stop intent is unconditional, including terminal repeat"
        );
        assert!(matches!(
            lock(&credits.state).slots.first(),
            Some(CreditSlot::Revoked)
        ));
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "member 1 channel 1 state ended accepted 102 consumed 102 tail complete\n"
        );
    }

    #[test]
    fn endpoint_return_in_serving_or_failed_drain_revokes_stops_and_publishes() {
        for (name, life, expected) in [
            ("lost-serving", Life::Serving, Life::Aborted(None)),
            (
                "lost-normal",
                Life::Draining(Terminal::End),
                Life::Aborted(Some(Terminal::End)),
            ),
            (
                "lost-capture",
                Life::Draining(Terminal::Aborted(5)),
                Life::Aborted(Some(Terminal::Aborted(5))),
            ),
        ] {
            let (output, credits, path) = fixture(name, life, State::Aborted(at(102, 86)), true);
            output.abort();
            assert_eq!(*lock(&output.life), expected);
            assert_eq!(output.control.stops.load(Ordering::SeqCst), 1);
            assert!(matches!(
                lock(&credits.state).slots.first(),
                Some(CreditSlot::Revoked)
            ));
            let row = std::fs::read_to_string(path).unwrap();
            assert!(
                row.ends_with("member 1 channel 1 state aborted accepted 102 consumed 86 tail unknown\n")
            );
            assert_eq!(row.contains("canonical aborted errno 5"), name == "lost-capture");
        }
    }

    #[test]
    fn metadata_write_failure_reports_scope_and_preserves_stop_progress() {
        lock(&LOGGED).clear();
        let (output, credits, path) = fixture(
            "metadata-readonly",
            Life::Draining(Terminal::End),
            State::Finished(at(102, 102)),
            false,
        );
        output.abort();
        assert_eq!(*lock(&output.life), Life::Ended);
        assert_eq!(
            output.control.stops.load(Ordering::SeqCst),
            2,
            "initial stop then failed-publication stop intent"
        );
        assert!(matches!(
            lock(&credits.state).slots.first(),
            Some(CreditSlot::Revoked)
        ));
        assert!(std::fs::read(path).unwrap().is_empty());
        let logged = lock(&LOGGED);
        assert_eq!(logged.len(), 1);
        assert!(
            logged
                .first()
                .unwrap()
                .starts_with("managed output metadata member 1 channel 1:")
        );
    }

    #[test]
    fn finish_releases_life_and_credit_before_inline_callback() {
        let (output, credits, _) = fixture("finish-locks", Life::Serving, State::Pending(at(102, 86)), true);
        let weak = Arc::downgrade(&output);
        let credit = Arc::clone(&credits);
        *lock(&output.control.on_finish) = Some(Box::new(move || {
            let output = weak.upgrade().unwrap();
            assert!(output.life.try_lock().is_ok());
            assert!(credit.state.try_lock().is_ok());
            credit.wake();
        }));
        output.finish(Terminal::End).unwrap();
        assert_eq!(*lock(&output.life), Life::Draining(Terminal::End));
        output.abort();
        *lock(&output.control.snapshot) = State::Finished(at(102, 102));
        output.abort();
        assert_eq!(*lock(&output.life), Life::Aborted(Some(Terminal::End)));
        assert!(output.finish(Terminal::End).is_err());
    }
}
