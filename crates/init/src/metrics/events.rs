use super::otlp::CgroupEvent;
use super::{Failure, Reader, Token, number};
use std::fs::File;

enum State {
    Key,
    Value(usize),
    End(usize),
    Other,
}

pub(super) fn read(reader: &mut Reader, group: &File) -> [Result<u64, Failure>; CgroupEvent::ALL.len()] {
    let mut values = [None; CgroupEvent::ALL.len()];
    // These base files follow the kernel's configured aggregation, including
    // memory_localevents/pids_localevents. They are not process observations.
    for name in [c"memory.events", c"pids.events"] {
        let selected = |event: CgroupEvent| (event == CgroupEvent::TaskMax) == (name == c"pids.events");
        let mut state = State::Key;
        let result = reader.tokens(group, name, |token| {
            match (&state, token) {
                (_, Token::Line) => state = State::Key,
                (State::Key, Token::Word(word)) => {
                    let found = CgroupEvent::ALL
                        .iter()
                        .position(|&event| selected(event) && event.key().as_bytes() == word);
                    state = if let Some(at) = found {
                        let slot = values.get_mut(at).ok_or(Failure::Invalid)?;
                        let duplicate = slot.is_some();
                        // A selected key with no value is invalid, not zero.
                        *slot = Some(Err(Failure::Invalid));
                        if duplicate { State::Other } else { State::Value(at) }
                    } else {
                        State::Other
                    };
                }
                (State::Key, Token::Long(_)) => state = State::Other,
                (State::Value(at), token) => {
                    let value = match token {
                        Token::Word(word) => number(word),
                        Token::Long(_) => Err(Failure::Overflow),
                        Token::Line => return Err(Failure::Invalid),
                    };
                    *values.get_mut(*at).ok_or(Failure::Invalid)? = Some(value);
                    state = State::End(*at);
                }
                (State::End(at), Token::Word(_) | Token::Long(_)) => {
                    *values.get_mut(*at).ok_or(Failure::Invalid)? = Some(Err(Failure::Invalid));
                    state = State::Other;
                }
                (State::Other, _) => {}
            }
            Ok(())
        });
        if let Err(cause) = result {
            for (event, slot) in CgroupEvent::ALL.into_iter().zip(&mut values) {
                if selected(event) {
                    *slot = Some(Err(cause));
                }
            }
        }
    }
    values.map(|value| value.unwrap_or(Err(Failure::Missing)))
}
