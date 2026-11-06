//! Wire compression (OP_COMPRESSED; mongoc-compression.c, mcd-rpc's compress/decompress):
//! snappy (snap), zlib (flate2 on miniz_oxide) and zstd (ruzstd), all pure Rust.
//!
//! Decompression is bounded: the declared uncompressed size must fit the message size limit,
//! and the output must be exactly that size (a stream that produces more is refused when it
//! reaches the limit, never buffered whole).

use std::io::Read;

pub const NOOP: u8 = 0;
pub const SNAPPY: u8 = 1;
pub const ZLIB: u8 = 2;
pub const ZSTD: u8 = 3;

/// mongoc_compressor_name_to_id (the compressors this build supports; case-insensitive).
pub fn id_of(name: &str) -> Option<u8> {
    [("snappy", SNAPPY), ("zlib", ZLIB), ("zstd", ZSTD), ("noop", NOOP)].into_iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, id)| id)
}

/// mongoc_server_description_compressor_id: the first compressor of the server's
/// `compression` reply that this build supports (noop included, as libmongoc).
pub fn negotiate(server: &[String]) -> Option<u8> {
    server.iter().find_map(|n| id_of(n))
}

/// mongoc_cmd_is_compressible: the handshake and authentication commands are never compressed.
pub fn is_compressible(command: &[u8]) -> bool {
    const NEVER: &[&[u8]] = &[b"hello", b"isMaster", b"authenticate", b"getnonce", b"saslstart", b"saslcontinue", b"createuser", b"updateuser"];
    !NEVER.iter().any(|n| n.eq_ignore_ascii_case(command))
}

/// The first key of a command document (its name); empty when there is none.
pub fn command_name(body: &[u8]) -> &[u8] {
    let k = body.get(5..).unwrap_or_default();
    &k[..k.iter().position(|&c| c == 0).unwrap_or(0)]
}

type ZstdCompressor = ruzstd::encoding::FrameCompressor<std::io::Cursor<Vec<u8>>, Vec<u8>, ruzstd::encoding::MatchGeneratorDriver>;

thread_local! {
    /// The zlib and zstd compressor states, kept between messages (their tables are large:
    /// allocating them per message would double the CPU cost of a small command).
    static ZLIB_STATE: std::cell::RefCell<Option<(u32, flate2::Compress)>> = const { std::cell::RefCell::new(None) };
    static ZSTD_STATE: std::cell::RefCell<Option<ZstdCompressor>> = const { std::cell::RefCell::new(None) };
}

fn zlib_compress(level: u32, data: &[u8]) -> Result<Vec<u8>, String> {
    ZLIB_STATE.with(|st| {
        let mut st = st.borrow_mut();
        if !matches!(&*st, Some((l, _)) if *l == level) {
            *st = Some((level, flate2::Compress::new(flate2::Compression::new(level), true)));
        }
        let c = &mut st.as_mut().unwrap().1;
        c.reset();
        let mut out = Vec::with_capacity(data.len() / 2 + 64);
        loop {
            let consumed = c.total_in() as usize;
            match c.compress_vec(&data[consumed..], &mut out, flate2::FlushCompress::Finish) {
                Ok(flate2::Status::StreamEnd) => return Ok(out),
                Ok(_) => out.reserve(out.capacity().max(64)),
                Err(e) => {
                    *st = None;
                    return Err(e.to_string());
                }
            }
        }
    })
}

fn zstd_compress(data: &[u8]) -> Vec<u8> {
    ZSTD_STATE.with(|st| {
        let mut st = st.borrow_mut();
        let c = st.get_or_insert_with(|| ruzstd::encoding::FrameCompressor::new(ruzstd::encoding::CompressionLevel::Fastest));
        c.set_source(std::io::Cursor::new(data.to_vec()));
        c.set_drain(Vec::with_capacity(data.len() / 2 + 64));
        c.compress();
        c.take_source();
        c.take_drain().unwrap_or_default()
    })
}

/// Compresses `data` (the message after its 16-byte header). `zlib_level`: -1 (zlib's
/// default, 6) to 9.
pub fn compress(id: u8, zlib_level: i32, data: &[u8]) -> Result<Vec<u8>, String> {
    match id {
        SNAPPY => snap::raw::Encoder::new().compress_vec(data).map_err(|e| e.to_string()),
        ZLIB => zlib_compress(if zlib_level < 0 { 6 } else { zlib_level.min(9) as u32 }, data),
        ZSTD => Ok(zstd_compress(data)),
        NOOP => Ok(data.to_vec()),
        _ => Err(format!("unknown compressor ID {id}")),
    }
}

/// Reads at most `size` + 1 bytes of `r`: exactly `size` are required.
fn read_exact_size(mut r: impl Read, size: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(size);
    (&mut r).take(size as u64 + 1).read_to_end(&mut out).map_err(|e| e.to_string())?;
    if out.len() != size {
        return Err(format!("decompressed {} bytes, expected {size}", out.len()));
    }
    Ok(out)
}

/// Decompresses to exactly `size` bytes (the uncompressedSize of the OP_COMPRESSED header,
/// already checked against the message size limit).
pub fn decompress(id: u8, data: &[u8], size: usize) -> Result<Vec<u8>, String> {
    match id {
        SNAPPY => {
            let n = snap::raw::decompress_len(data).map_err(|e| e.to_string())?;
            if n != size {
                return Err(format!("snappy data of {n} bytes, expected {size}"));
            }
            let mut out = vec![0u8; size];
            let w = snap::raw::Decoder::new().decompress(data, &mut out).map_err(|e| e.to_string())?;
            if w != size {
                return Err(format!("decompressed {w} bytes, expected {size}"));
            }
            Ok(out)
        }
        ZLIB => read_exact_size(flate2::read::ZlibDecoder::new(data), size),
        ZSTD => {
            // the window the frame may ask for is bounded by what it may produce: libzstd sizes
            // the window of a frame to the power of two that covers its content
            let d = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(data, (size as u64).next_power_of_two().max(1 << 20)).map_err(|e| e.to_string())?;
            read_exact_size(d, size)
        }
        NOOP => {
            if data.len() != size {
                return Err(format!("noop data of {} bytes, expected {size}", data.len()));
            }
            Ok(data.to_vec())
        }
        _ => {
            crate::log::warning(crate::log::MONGOC, || format!("Unknown compressor ID {id}"));
            Err(format!("unknown compressor ID {id}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let data: Vec<u8> = (0..100_000u32).flat_map(|i| (i % 251).to_le_bytes()).collect();
        for id in [SNAPPY, ZLIB, ZSTD, NOOP] {
            for level in [-1, 0, 1, 9] {
                let c = compress(id, level, &data).unwrap();
                assert_eq!(decompress(id, &c, data.len()).unwrap(), data, "id {id} level {level}");
            }
        }
        assert!(compress(ZLIB, -1, &data).unwrap().len() < data.len() / 4);
    }

    #[test]
    fn sizes_are_exact_and_bounded() {
        let data = vec![7u8; 1 << 20];
        for id in [SNAPPY, ZLIB, ZSTD, NOOP] {
            let c = compress(id, -1, &data).unwrap();
            assert!(decompress(id, &c, data.len() - 1).is_err(), "id {id}: more output than declared");
            assert!(decompress(id, &c, data.len() + 1).is_err(), "id {id}: less output than declared");
            assert!(decompress(id, &c[..c.len() / 2], data.len()).is_err(), "id {id}: truncated");
        }
        // a zlib bomb: 64 MiB of zeros declared as 1 KiB stops after 1 KiB + 1 of output
        let bomb = compress(ZLIB, 9, &vec![0u8; 64 << 20]).unwrap();
        assert!(decompress(ZLIB, &bomb, 1024).is_err());
        assert!(decompress(9, b"x", 1).is_err());
        assert!(decompress(ZSTD, b"garbage", 7).is_err());
    }

    #[test]
    fn negotiation_and_compressibility() {
        assert_eq!(negotiate(&["zstd".into(), "zlib".into()]), Some(ZSTD));
        assert_eq!(negotiate(&["lz4".into(), "snappy".into(), "noop".into()]), Some(SNAPPY));
        assert_eq!(negotiate(&["lz4".into(), "NOOP".into(), "snappy".into()]), Some(NOOP));
        assert_eq!(negotiate(&[]), None);
        assert!(!is_compressible(b"hello") && !is_compressible(b"saslStart") && !is_compressible(b"createUser") && !is_compressible(b"ismaster"));
        assert!(is_compressible(b"find") && is_compressible(b"insert"));
        assert_eq!(command_name(b"\x13\x00\x00\x00\x10find\x00\x01\x00\x00\x00\x00"), b"find");
        assert_eq!(command_name(b"\x05\x00\x00\x00\x00"), b"");
    }

    #[test]
    fn op_compressed_messages() {
        use crate::wire;
        let doc = b"\x1b\x00\x00\x00\x02find\x00\x0c\x00\x00\x00aaaaaaaaaaa\x00\x00";
        let mut m = wire::MsgBuilder::new(77, 0);
        m.body(doc);
        let msg = m.finish();
        for id in [SNAPPY, ZLIB, ZSTD, NOOP] {
            let c = wire::compress_message(&msg, id, -1).unwrap();
            assert_eq!(wire::op_code(&c), wire::OP_COMPRESSED);
            assert_eq!(wire::message_length(&c) as usize, c.len());
            assert_eq!(&c[4..8], &77i32.to_le_bytes());
            assert_eq!(c[24], id);
            // a reply in the same form decompresses to the original message
            let d = wire::decompress_message(&c, 1 << 20).unwrap();
            assert_eq!(d, msg, "id {id}");
            assert_eq!(wire::parse_reply(&d).unwrap().body, &doc[..]);
            // the declared size must fit the limit and match the data
            assert_eq!(wire::decompress_message(&c, msg.len() - 1), Err(wire::DECOMPRESS_ERROR));
            for delta in [-1i32, 1, i32::MIN] {
                let mut bad = c.clone();
                let size = i32::from_le_bytes(bad[20..24].try_into().unwrap()).wrapping_add(delta);
                let size = if delta == i32::MIN { -5 } else { size };
                bad[20..24].copy_from_slice(&size.to_le_bytes());
                assert_eq!(wire::decompress_message(&bad, 1 << 20), Err(wire::DECOMPRESS_ERROR), "id {id} delta {delta}");
            }
            let mut unknown = c.clone();
            unknown[24] = 9;
            assert_eq!(wire::decompress_message(&unknown, 1 << 20), Err(wire::DECOMPRESS_ERROR));
            assert_eq!(wire::decompress_message(&c[..c.len() - 1], 1 << 20), Err(wire::DECOMPRESS_ERROR));
        }
        // a declared size above the limit is refused before anything is allocated or decoded
        let big = compress(ZLIB, 9, &vec![0u8; 8 << 20]).unwrap();
        let mut m = Vec::new();
        m.extend_from_slice(&((25 + big.len()) as i32).to_le_bytes());
        m.extend_from_slice(&[0; 8]);
        m.extend_from_slice(&wire::OP_COMPRESSED.to_le_bytes());
        m.extend_from_slice(&wire::OP_MSG.to_le_bytes());
        m.extend_from_slice(&((8 << 20) as i32).to_le_bytes());
        m.push(ZLIB);
        m.extend_from_slice(&big);
        assert_eq!(wire::decompress_message(&m, 1 << 20), Err(wire::DECOMPRESS_ERROR));
        assert!(wire::decompress_message(&m, 16 << 20).is_ok());
    }
}
