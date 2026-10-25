//! Small runtime primitives shared by Kwil's existing worker owners.
//!
//! Native-exit querying owns no worker, source, executor or allocation. The
//! Windows candidate is isolated until ROOT approves platform integration.
#![forbid(unsafe_op_in_unsafe_fn)]

mod native_exit;

pub use native_exit::{NativeExitQueryError, NativeExitState, native_exit_query_stack_bytes, native_exit_state};
