//! JSONL framing for bytes supplied by the selected native pipe's sole owner.
//! Storage is initialized and admitted by that owner; this module never reads a
//! descriptor, grows storage, parses output from ordinary launches or issues rights.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    Newline,
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    InitialStorage,
    State,
    Capacity { capacity: usize },
    Length,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Receiving,
    Ready(End),
    Closed,
    Failed(Failure),
}

pub(crate) struct Rejected {
    pub failure: Failure,
    pub storage: Vec<u8>,
}

pub(crate) struct Line {
    bytes: Vec<u8>,
    state: State,
}

impl Line {
    pub fn attach(storage: Vec<u8>) -> Result<Self, Rejected> {
        if !storage.is_empty() {
            return Err(Rejected {
                failure: Failure::InitialStorage,
                storage,
            });
        }
        Ok(Self {
            bytes: storage,
            state: State::Receiving,
        })
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// Consumes at most the first line, including its LF. The caller retains
    /// every unconsumed suffix. Whitespace, CR and arbitrary bytes stay exact;
    /// JSON validation and raw capture belong to their existing owners.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<usize, Failure> {
        match self.state {
            State::Receiving => {}
            State::Failed(failure) => return Err(failure),
            State::Ready(_) | State::Closed => return Err(Failure::State),
        }
        let newline = bytes.iter().position(|&byte| byte == b'\n');
        let count = newline.unwrap_or(bytes.len());
        let body = bytes.get(..count).ok_or(Failure::Length)?;
        let Some(length) = self.bytes.len().checked_add(body.len()) else {
            self.state = State::Failed(Failure::Length);
            return Err(Failure::Length);
        };
        if length > self.bytes.capacity() {
            let failure = Failure::Capacity {
                capacity: self.bytes.capacity(),
            };
            self.state = State::Failed(failure);
            return Err(failure);
        }
        // The check above prevents Vec's append from reallocating.
        self.bytes.extend_from_slice(body);
        match newline {
            Some(_) => {
                let consumed = count.checked_add(1).ok_or(Failure::Length)?;
                self.state = State::Ready(End::Newline);
                Ok(consumed)
            }
            None => Ok(count),
        }
    }

    pub fn line(&self) -> Option<&[u8]> {
        matches!(self.state, State::Ready(_)).then_some(self.bytes.as_slice())
    }

    /// EOF makes a remaining tail available for the same parser. It cannot
    /// certify that the tail is valid JSON, that an operation ended or was reaped.
    pub fn eof(&mut self) -> Result<State, Failure> {
        match self.state {
            State::Receiving => {
                self.state = if self.bytes.is_empty() {
                    State::Closed
                } else {
                    State::Ready(End::Eof)
                };
                Ok(self.state)
            }
            State::Closed => Ok(State::Closed),
            State::Failed(failure) => Err(failure),
            State::Ready(_) => Err(Failure::State),
        }
    }

    /// Release only after the consumer has retained its decision/reply. There is
    /// no reconnect, replay or transition out of a capacity failure here.
    pub fn release(&mut self) -> Result<State, Failure> {
        let next = match self.state {
            State::Ready(End::Newline) => State::Receiving,
            State::Ready(End::Eof) => State::Closed,
            State::Failed(failure) => return Err(failure),
            State::Receiving | State::Closed => return Err(Failure::State),
        };
        self.bytes.clear();
        self.state = next;
        Ok(next)
    }

    pub fn into_storage(self) -> Vec<u8> {
        self.bytes
    }
}
