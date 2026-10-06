//! Platform back ends. Each exposes the same free functions; the generic
//! logic in `lib.rs` never touches paths by name after resolution.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
