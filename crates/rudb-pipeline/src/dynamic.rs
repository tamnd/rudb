//! Erasing `Local` so that a pipeline can hold operators of different shapes in one list.
//!
//! [`Stream`] and [`Sink`] carry an associated type, which is what lets somebody write an operator
//! with real typed state and no casting anywhere in it. It is also what stops a pipeline from
//! holding them in a `Vec`, because two streams with different local state are two different
//! types.
//!
//! The fix is the usual one. The typed traits are what an operator author implements, and a
//! blanket implementation gives every one of them an erased twin that the driver calls. The cast
//! back is once per chunk per operator, which is the granularity rule the whole design is built
//! on, and it cannot fail in practice because the same object produced the state it is handed.

use std::any::Any;
use std::fmt;

use rudb_common::{Error, Result};
use rudb_vector::Chunk;

use crate::progress::Progress;
use crate::traits::{Sink, Stream};

/// One operator's per instance state, with its type forgotten.
pub struct LocalState(Box<dyn Any + Send>);

impl LocalState {
    /// Wrap some state.
    #[must_use]
    pub fn new<T: Send + 'static>(state: T) -> Self {
        Self(Box::new(state))
    }

    /// Borrow it back as what it was.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) when the state belongs to a
    /// different operator. That is a driver bug rather than anything a query can cause, and it is
    /// an error rather than a panic because an engine that aborts on its own bug takes the user's
    /// session with it.
    pub fn downcast_mut<T: Send + 'static>(&mut self) -> Result<&mut T> {
        self.0.downcast_mut::<T>().ok_or_else(|| {
            Error::internal("operator was handed local state belonging to another operator")
        })
    }

    /// Take it back as what it was.
    ///
    /// # Errors
    ///
    /// The same as [`LocalState::downcast_mut`].
    pub fn downcast<T: Send + 'static>(self) -> Result<T> {
        match self.0.downcast::<T>() {
            Ok(state) => Ok(*state),
            Err(_) => Err(Error::internal(
                "operator was handed local state belonging to another operator",
            )),
        }
    }
}

impl fmt::Debug for LocalState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LocalState")
    }
}

/// [`Stream`] with its local state type erased. Implemented for every [`Stream`], never by hand.
pub trait DynStream: Send + Sync + fmt::Debug {
    /// Fresh local state for one instance.
    fn local_state(&self) -> LocalState;

    /// Transform `chunk` in place.
    ///
    /// # Errors
    ///
    /// Whatever the typed operator reports, plus an internal error if the state is the wrong one.
    fn push_state(&self, chunk: &mut Chunk, local: &mut LocalState) -> Result<Progress>;
}

impl<S: Stream> DynStream for S {
    fn local_state(&self) -> LocalState {
        LocalState::new(self.local())
    }

    fn push_state(&self, chunk: &mut Chunk, local: &mut LocalState) -> Result<Progress> {
        self.push(chunk, local.downcast_mut::<S::Local>()?)
    }
}

/// [`Sink`] with its local state type erased. Implemented for every [`Sink`], never by hand.
pub trait DynSink: Send + Sync + fmt::Debug {
    /// Fresh local state for one instance.
    fn local_state(&self) -> LocalState;

    /// Take one chunk into the local state.
    ///
    /// # Errors
    ///
    /// Whatever the typed operator reports, plus an internal error if the state is the wrong one.
    fn sink_state(&self, chunk: &Chunk, local: &mut LocalState) -> Result<Progress>;

    /// Merge one instance's local state into the global state.
    ///
    /// # Errors
    ///
    /// Whatever the typed operator reports, plus an internal error if the state is the wrong one.
    fn combine_state(&self, local: LocalState) -> Result<()>;

    /// Called once, after every merge.
    ///
    /// # Errors
    ///
    /// Whatever the typed operator reports.
    fn finalize_state(&self) -> Result<()>;
}

impl<S: Sink> DynSink for S {
    fn local_state(&self) -> LocalState {
        LocalState::new(self.local())
    }

    fn sink_state(&self, chunk: &Chunk, local: &mut LocalState) -> Result<Progress> {
        self.sink(chunk, local.downcast_mut::<S::Local>()?)
    }

    fn combine_state(&self, local: LocalState) -> Result<()> {
        self.combine(local.downcast::<S::Local>()?)
    }

    fn finalize_state(&self) -> Result<()> {
        self.finalize()
    }
}
