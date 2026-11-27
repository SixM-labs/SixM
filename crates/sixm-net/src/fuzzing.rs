//! Entry points for the fuzz targets in `fuzz/` and the stable fuzz test.
//!
//! Each function runs one parser on untrusted bytes and reports only whether
//! it succeeded. This module is hidden from the documentation and is not a
//! stable API.

use crate::error::NetError;
use crate::files::FileHash;
use crate::latent::{HEADER_LEN as LATENT_HEADER_LEN, parse_latent_header};
use crate::transfer::{HEADER_LEN, parse_file_header};
use crate::wire;

/// Decodes a payload sent by a client.
pub fn decode_client_message(payload: &[u8]) -> Result<(), NetError> {
    wire::decode_client_message(payload).map(|_| ())
}

/// Decodes a payload sent by the server.
pub fn decode_server_message(payload: &[u8]) -> Result<(), NetError> {
    wire::decode_server_message(payload).map(|_| ())
}

/// Decodes the body of a latent event.
pub fn decode_latent_body(payload: &[u8]) -> Result<(), NetError> {
    wire::decode_latent_body(payload).map(|_| ())
}

/// Parses the header of a file stream. `None` when `header` has the wrong length.
pub fn parse_file_stream_header(header: &[u8]) -> Option<(FileHash, u64)> {
    let header: &[u8; HEADER_LEN] = header.try_into().ok()?;
    Some(parse_file_header(header))
}

/// Parses the header of a latent stream, after its kind byte. `None` when
/// `header` has the wrong length.
pub fn parse_latent_stream_header(header: &[u8]) -> Option<(u64, usize)> {
    let header: &[u8; LATENT_HEADER_LEN] = header.try_into().ok()?;
    Some(parse_latent_header(header))
}
