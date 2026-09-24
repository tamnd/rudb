//! The status a pipeline step returns, per section 5.4.1 of `spec/compiler/05-pipelines.md`:
//! the kind in the low 8 bits and a payload in the high 56.
//!
//! Every tier builds these values the same way, so the driver reads one encoding whatever ran
//! the step.

/// The morsel is done; go on.
pub const OK: u64 = 0;
/// Call the body again with the same morsel; the cursor is in local state.
pub const YIELD: u64 = 1;
/// The pipeline may stop early: a LIMIT was reached.
pub const DONE: u64 = 2;
/// A guard failed. The payload is the guard id.
pub const DEOPT: u64 = 3;
/// The body needs memory. The payload is the slot id.
pub const NEED_MEMORY: u64 = 4;
/// The query is being cancelled.
pub const CANCELLED: u64 = 5;
/// The query fails. The payload is the error site id.
pub const ERROR: u64 = 6;

/// A status of kind `kind` carrying `payload`.
#[must_use]
pub fn make(kind: u64, payload: u64) -> u64 {
    kind | (payload << 8)
}

/// The kind of a status.
#[must_use]
pub fn kind(status: u64) -> u64 {
    status & 0xff
}

/// The payload of a status.
#[must_use]
pub fn payload(status: u64) -> u64 {
    status >> 8
}
