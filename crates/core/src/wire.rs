//! The wire protocol: OP_MSG messages (with document sequences) and their replies.
//! See the OP_MSG specification and libmongoc's mcd-rpc.c / mongoc-cmd.c.

/// OP_MSG document sequences (section kind 1): (identifier, the concatenated documents).
pub type Payloads<'a> = &'a [(&'a str, &'a [u8])];

/// OP_MSG
pub const OP_MSG: i32 = 2013;
/// OP_COMPRESSED
pub const OP_COMPRESSED: i32 = 2012;
/// OP_REPLY (legacy replies, never requested by this driver)
pub const OP_REPLY: i32 = 1;
/// The header: messageLength, requestID, responseTo, opCode.
pub const HEADER_LEN: usize = 16;

/// flagBits: checksumPresent
pub const FLAG_CHECKSUM_PRESENT: u32 = 1;
/// flagBits: moreToCome
pub const FLAG_MORE_TO_COME: u32 = 1 << 1;
/// flagBits: exhaustAllowed
pub const FLAG_EXHAUST_ALLOWED: u32 = 1 << 16;

/// An OP_MSG under construction: header, flagBits, the section-0 body, then any kind-1 sections.
pub struct MsgBuilder {
    pub buf: Vec<u8>,
}

impl MsgBuilder {
    /// Starts a message with `request_id` and `flags`; append the body with `body`.
    pub fn new(request_id: i32, flags: u32) -> Self {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(&[0; 4]);
        buf.extend_from_slice(&request_id.to_le_bytes());
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(&OP_MSG.to_le_bytes());
        buf.extend_from_slice(&flags.to_le_bytes());
        MsgBuilder { buf }
    }

    /// Section kind 0: the command body (a complete BSON document).
    pub fn body(&mut self, doc: &[u8]) {
        self.buf.push(0);
        self.buf.extend_from_slice(doc);
    }

    /// Section kind 1: a document sequence named `id` (e.g. "documents", "updates").
    pub fn sequence<'a>(&mut self, id: &str, docs: impl IntoIterator<Item = &'a [u8]>) {
        self.buf.push(1);
        let at = self.buf.len();
        self.buf.extend_from_slice(&[0; 4]);
        self.buf.extend_from_slice(id.as_bytes());
        self.buf.push(0);
        for d in docs {
            self.buf.extend_from_slice(d);
        }
        let n = (self.buf.len() - at) as i32;
        self.buf[at..at + 4].copy_from_slice(&n.to_le_bytes());
    }

    /// Section kind 1 from already concatenated documents.
    pub fn sequence_raw(&mut self, id: &str, docs: &[u8]) {
        self.buf.push(1);
        let n = (4 + id.len() + 1 + docs.len()) as i32;
        self.buf.extend_from_slice(&n.to_le_bytes());
        self.buf.extend_from_slice(id.as_bytes());
        self.buf.push(0);
        self.buf.extend_from_slice(docs);
    }

    /// Patches messageLength and returns the message.
    pub fn finish(mut self) -> Vec<u8> {
        let n = self.buf.len() as i32;
        self.buf[0..4].copy_from_slice(&n.to_le_bytes());
        self.buf
    }
}

/// A parsed OP_MSG reply (borrowing the received bytes).
#[derive(Debug)]
pub struct Reply<'a> {
    pub request_id: i32,
    pub response_to: i32,
    pub flags: u32,
    /// The section-0 document.
    pub body: &'a [u8],
    /// Kind-1 sections: (identifier, concatenated documents).
    pub sequences: Vec<(&'a [u8], &'a [u8])>,
}

#[inline]
fn rd_i32(b: &[u8], at: usize) -> i32 {
    i32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// The messageLength of a header (at least 4 bytes).
pub fn message_length(b: &[u8]) -> i32 {
    rd_i32(b, 0)
}

/// The opCode of a message (at least 16 bytes).
pub fn op_code(b: &[u8]) -> i32 {
    rd_i32(b, 12)
}

/// Parses a complete OP_MSG; Err with a description when it is malformed.
pub fn parse_reply(m: &[u8]) -> Result<Reply<'_>, &'static str> {
    if m.len() < HEADER_LEN + 5 || message_length(m) as usize != m.len() {
        return Err("invalid reply length");
    }
    if op_code(m) != OP_MSG {
        return Err("unexpected reply opcode");
    }
    let flags = u32::from_le_bytes(m[16..20].try_into().unwrap());
    let end = if flags & FLAG_CHECKSUM_PRESENT != 0 { m.len() - 4 } else { m.len() };
    let mut at = 20;
    let mut body = None;
    let mut sequences = Vec::new();
    while at < end {
        let kind = m[at];
        at += 1;
        if at + 4 > end {
            return Err("truncated section");
        }
        let n = rd_i32(m, at);
        if n < 5 || at + n as usize > end {
            return Err("invalid section length");
        }
        let n = n as usize;
        match kind {
            0 => {
                if body.is_some() {
                    return Err("more than one body section");
                }
                body = Some(&m[at..at + n]);
            }
            1 => {
                let s = &m[at + 4..at + n];
                let Some(z) = s.iter().position(|&c| c == 0) else { return Err("unterminated sequence identifier") };
                sequences.push((&s[..z], &s[z + 1..]));
            }
            _ => return Err("unknown section kind"),
        }
        at += n;
    }
    let Some(body) = body else { return Err("missing body section") };
    Ok(Reply { request_id: rd_i32(m, 4), response_to: rd_i32(m, 8), flags, body, sequences })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bson::writer::Writer;

    #[test]
    fn roundtrip() {
        let mut w = Writer::new();
        w.int32(b"ping", 1);
        let body = w.finish();
        let mut d = Writer::new();
        d.int32(b"x", 1);
        let d = d.finish();
        let mut b = MsgBuilder::new(7, 0);
        b.body(&body);
        b.sequence("documents", [&d[..], &d[..]]);
        let m = b.finish();
        let r = parse_reply(&m).unwrap();
        assert_eq!(r.request_id, 7);
        assert_eq!(r.body, &body[..]);
        assert_eq!(r.sequences, vec![(&b"documents"[..], &[&d[..], &d[..]].concat()[..])]);
    }
}

/// The OP_COMPRESSED form of a complete message (requestID kept, responseTo 0).
pub fn compress_message(msg: &[u8], id: u8, zlib_level: i32) -> Result<Vec<u8>, String> {
    let data = crate::compress::compress(id, zlib_level, &msg[HEADER_LEN..])?;
    let len = i32::try_from(HEADER_LEN + 9 + data.len()).map_err(|_| "message too large".to_owned())?;
    let mut out = Vec::with_capacity(len as usize);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&msg[4..8]);
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&OP_COMPRESSED.to_le_bytes());
    out.extend_from_slice(&op_code(msg).to_le_bytes());
    out.extend_from_slice(&((msg.len() - HEADER_LEN) as i32).to_le_bytes());
    out.push(id);
    out.extend_from_slice(&data);
    Ok(out)
}

/// libmongoc's error for every reply that cannot be decompressed (MONGOC_ERROR_PROTOCOL_INVALID_REPLY).
pub const DECOMPRESS_ERROR: &str = "could not decompress message from server";

/// The message an OP_COMPRESSED reply carries (header rebuilt with the original opcode;
/// mcd_rpc_message_decompress). The uncompressed size must keep the message within `max`
/// (the message size limit) and the data must decompress to exactly that size.
pub fn decompress_message(m: &[u8], max: usize) -> Result<Vec<u8>, &'static str> {
    if m.len() < HEADER_LEN + 9 || message_length(m) as usize != m.len() {
        return Err(DECOMPRESS_ERROR);
    }
    let original = rd_i32(m, 16);
    let size = rd_i32(m, 20);
    if size < 0 || HEADER_LEN + size as usize > max {
        return Err(DECOMPRESS_ERROR);
    }
    let body = crate::compress::decompress(m[24], &m[25..], size as usize).map_err(|_| DECOMPRESS_ERROR)?;
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(&((HEADER_LEN + body.len()) as i32).to_le_bytes());
    out.extend_from_slice(&m[4..12]);
    out.extend_from_slice(&original.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}
