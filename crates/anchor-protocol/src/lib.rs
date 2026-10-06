//! Wire protocol between the Anchor backend (Layer 1) and the agent runtime
//! (Layer 2).
//!
//! Transport: newline-delimited JSON over the runtime's stdio. Every line is
//! one [`Frame`]. Both ends:
//!
//! * reject frames larger than [`MAX_FRAME_BYTES`],
//! * reject frames whose `v` is not [`PROTOCOL_VERSION`],
//! * reject unknown fields (`deny_unknown_fields` on every type),
//! * run [`Validate::validate`] on the decoded value before acting on it.
//!
//! The protocol is deliberately narrow (spec §4). The runtime can *request*
//! tool executions; it cannot express anything the backend would act on
//! without passing the request through the permission engine.

use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};

mod messages;
pub use messages::*;

/// Wire protocol version. Bump on any incompatible change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Hard upper bound on the size of one encoded frame, including the newline.
pub const MAX_FRAME_BYTES: usize = 16 << 20;

/// Maximum length of any path-like or pattern-like string parameter.
pub const MAX_PARAM_BYTES: usize = 4096;

/// Maximum length of file content carried in a single frame.
pub const MAX_CONTENT_BYTES: usize = 4 << 20;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame exceeds {MAX_FRAME_BYTES} bytes")]
    FrameTooLarge,
    #[error("frame is not valid UTF-8")]
    NotUtf8,
    #[error("malformed frame: {0}")]
    Malformed(String),
    #[error("protocol version mismatch: expected {expected}, got {got}")]
    VersionMismatch { expected: u32, got: u32 },
    #[error("invalid frame: {0}")]
    Invalid(String),
}

/// One message on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    /// Protocol version; must equal [`PROTOCOL_VERSION`].
    pub v: u32,
    /// Sender-assigned, strictly increasing sequence number.
    pub id: u64,
    pub msg: Message,
}

impl Frame {
    pub fn new(id: u64, msg: Message) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            msg,
        }
    }
}

/// Structural validation beyond what serde enforces.
pub trait Validate {
    fn validate(&self) -> Result<(), String>;
}

impl Validate for Frame {
    fn validate(&self) -> Result<(), String> {
        self.msg.validate()
    }
}

/// Encode one frame as a single line (without trailing newline).
pub fn encode(frame: &Frame) -> Result<String, ProtocolError> {
    frame.validate().map_err(ProtocolError::Invalid)?;
    let s = serde_json::to_string(frame).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    if s.len() + 1 > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    Ok(s)
}

/// Decode and fully validate one line.
pub fn decode(line: &str) -> Result<Frame, ProtocolError> {
    if line.len() + 1 > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    // Check the version before full decoding so a future-version peer gets a
    // precise error instead of an "unknown variant" one.
    #[derive(Deserialize)]
    struct VersionOnly {
        v: u32,
    }
    let v: VersionOnly =
        serde_json::from_str(line).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    if v.v != PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            got: v.v,
        });
    }
    let frame: Frame =
        serde_json::from_str(line).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    frame.validate().map_err(ProtocolError::Invalid)?;
    Ok(frame)
}

/// Write one frame followed by `\n`, then flush.
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> Result<(), ProtocolError> {
    let s = encode(frame)?;
    w.write_all(s.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(())
}

/// Read one frame. Returns `Ok(None)` on clean EOF.
///
/// Reads at most [`MAX_FRAME_BYTES`] before giving up, so a peer cannot make
/// us buffer an unbounded line.
pub fn read_frame<R: BufRead>(r: &mut R) -> Result<Option<Frame>, ProtocolError> {
    let mut buf = Vec::new();
    let mut limited = std::io::Read::take(r.by_ref(), MAX_FRAME_BYTES as u64);
    let n = limited.read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        if buf.len() >= MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge);
        }
        return Err(ProtocolError::Malformed("unterminated frame at EOF".into()));
    }
    buf.pop();
    let line = std::str::from_utf8(&buf).map_err(|_| ProtocolError::NotUtf8)?;
    decode(line).map(Some)
}

pub(crate) fn check_param(name: &str, s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!("{name} is empty"));
    }
    if s.len() > MAX_PARAM_BYTES {
        return Err(format!("{name} exceeds {MAX_PARAM_BYTES} bytes"));
    }
    if s.contains('\0') {
        return Err(format!("{name} contains NUL"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
