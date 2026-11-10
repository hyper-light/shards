//! Instance-bound teardown witnesses for one ordinary owned-API test.
//! Messages contain no Guest, Shared, guard, or native resource owner.

use std::sync::{OnceLock, mpsc::Sender};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Object {
    State,
    Changed,
    Tail,
    Drained,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Event {
    SharedDropEntered,
    DestroyEntered(Object),
    DestroyReturned { object: Object, code: i32 },
    PointeeDropReturned(Object),
    PointeeRetained(Object),
    OriginalSharedFieldsDropped,
}

#[derive(Debug)]
pub(crate) struct Watch {
    object: Object,
    sender: Sender<Event>,
}

impl Watch {
    pub(crate) fn new(object: Object, sender: Sender<Event>) -> Self {
        Self { object, sender }
    }

    pub(crate) fn entering(&self) {
        let _ = self.sender.send(Event::DestroyEntered(self.object));
    }

    pub(crate) fn returned(&self, code: i32) {
        let _ = self.sender.send(Event::DestroyReturned {
            object: self.object,
            code,
        });
    }

    /// Called after the existing release branch, never as its precondition.
    pub(crate) fn storage(&self, released: bool) {
        let event = if released {
            Event::PointeeDropReturned(self.object)
        } else {
            Event::PointeeRetained(self.object)
        };
        let _ = self.sender.send(event);
    }
}

#[derive(Default, Debug)]
pub(crate) struct SharedWitness(OnceLock<Sender<Event>>);

impl SharedWitness {
    pub(crate) fn attach(&self, sender: Sender<Event>) -> Result<(), Sender<Event>> {
        self.0.set(sender)
    }

    pub(crate) fn entering(&self) {
        if let Some(sender) = self.0.get() {
            let _ = sender.send(Event::SharedDropEntered);
        }
    }
}

impl Drop for SharedWitness {
    fn drop(&mut self) {
        if let Some(sender) = self.0.get() {
            // This field is last in Shared. Triomphe's enclosing allocation has
            // not yet been released at this observation point.
            let _ = sender.send(Event::OriginalSharedFieldsDropped);
        }
    }
}
