// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
use crate::{Error, Result};
use crispawn_core::{error::require, plan::DECODER_MEMORY, types::Codec};
use std::io::{Read, Write};

pub struct Encoded {
    pub codec: Codec,
    pub level: i32,
    pub payload: Vec<u8>,
}
pub fn encode(raw: Vec<u8>, codec: Codec, level: i32) -> Result<Encoded> {
    let _trace = crate::io_trace::span(crate::io_trace::Op::Encode, raw.len() as u64);
    if raw.iter().all(|v| *v == 0) {
        return Ok(Encoded {
            codec: Codec::Zero,
            level: 0,
            payload: vec![],
        });
    }
    let payload = match codec {
        Codec::Raw => {
            return Ok(Encoded {
                codec,
                level: 0,
                payload: raw,
            })
        }
        Codec::Zstd => {
            let mut c = zstd::bulk::Compressor::new(level)?;
            c.set_parameter(zstd::zstd_safe::CParameter::WindowLog(23))?;
            c.compress(&raw)?
        }
        Codec::Xz => {
            let mut c = xz2::write::XzEncoder::new(Vec::new(), level as u32);
            c.write_all(&raw)?;
            c.finish()?
        }
        Codec::Gzip => {
            let mut c =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level as u32));
            c.write_all(&raw)?;
            c.finish()?
        }
        Codec::Zero => {
            return Err(Error::new(
                "INVALID_CODEC",
                "zero is detected from actual source data",
            ))
        }
    };
    if payload.len() >= raw.len() {
        Ok(Encoded {
            codec: Codec::Raw,
            level: 0,
            payload: raw,
        })
    } else {
        Ok(Encoded {
            codec,
            level,
            payload,
        })
    }
}
pub fn decode(codec: Codec, payload: &[u8], length: usize) -> Result<Vec<u8>> {
    let out = match codec {
        Codec::Raw => payload.to_vec(),
        Codec::Zero => {
            require(payload.is_empty(), "INVALID_RECORD", "zero payload")?;
            vec![0; length]
        }
        Codec::Zstd => {
            let size = zstd::zstd_safe::find_frame_compressed_size(payload)
                .map_err(|_| Error::new("DECODE_ERROR", "invalid zstd frame"))?;
            require(
                size == payload.len(),
                "TRAILING_PAYLOAD",
                "zstd frame must consume entire payload",
            )?;
            check_zstd_window(payload, length)?;
            let mut d = zstd::bulk::Decompressor::new()?;
            d.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(23))?;
            d.decompress(payload, length)?
        }
        Codec::Xz => {
            let stream = xz2::stream::Stream::new_stream_decoder(DECODER_MEMORY, 0)
                .map_err(|e| Error::new("DECODE_ERROR", e.to_string()))?;
            let mut d = xz2::bufread::XzDecoder::new_stream(payload, stream);
            let mut b = Vec::new();
            d.by_ref().take(length as u64 + 1).read_to_end(&mut b)?;
            require(
                d.total_in() == payload.len() as u64,
                "TRAILING_PAYLOAD",
                "xz frame did not consume entire payload",
            )?;
            b
        }
        Codec::Gzip => {
            let mut d = flate2::bufread::GzDecoder::new(payload);
            let mut b = Vec::new();
            d.by_ref().take(length as u64 + 1).read_to_end(&mut b)?;
            require(
                d.get_ref().is_empty(),
                "TRAILING_PAYLOAD",
                "gzip frame did not consume entire payload",
            )?;
            b
        }
    };
    require(
        out.len() == length,
        "DECODE_LENGTH_MISMATCH",
        "decoded range length",
    )?;
    Ok(out)
}

// WindowLogMax is a streaming decoder limit. The bulk API does not enforce it,
// so validate the standard frame header as well as the bounded output length.
fn check_zstd_window(payload: &[u8], length: usize) -> Result<()> {
    require(
        payload.len() >= 6 && payload[..4] == [0x28, 0xb5, 0x2f, 0xfd],
        "INVALID_ZSTD_FRAME",
        "standard frame required",
    )?;
    let content = zstd::zstd_safe::get_frame_content_size(payload)
        .map_err(|_| Error::new("INVALID_ZSTD_FRAME", "frame content size"))?;
    if let Some(size) = content {
        require(
            size == length as u64,
            "DECODE_LENGTH_MISMATCH",
            "zstd frame content size",
        )?;
    }
    let window = if payload[4] & 0x20 != 0 {
        content.ok_or_else(|| {
            Error::new(
                "INVALID_ZSTD_FRAME",
                "single-segment frame without content size",
            )
        })?
    } else {
        let base = 1u64 << (10 + (payload[5] >> 3));
        base + (base / 8) * u64::from(payload[5] & 7)
    };
    require(
        window <= 8 << 20,
        "ZSTD_WINDOW_LIMIT",
        "v1 allows at most an 8 MiB window",
    )?;
    require(
        zstd::zstd_safe::get_dict_id_from_frame(payload).is_none(),
        "ZSTD_DICTIONARY_FORBIDDEN",
        "v1 frames must not depend on a dictionary",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bulk_decoder_enforces_v1_window_limit_explicitly() {
        let raw = vec![b'x'; 16 << 20];
        let e = encode(raw.clone(), Codec::Zstd, 3).unwrap();
        assert_eq!(e.payload[4] & 0x20, 0);
        let mut larger = e.payload.clone();
        larger[5] = 14 << 3; // 16 MiB advertised window.
                             // The upstream bulk API accepts it; our v1 reader must reject it.
        assert_eq!(zstd::bulk::decompress(&larger, raw.len()).unwrap(), raw);
        assert_eq!(
            decode(Codec::Zstd, &larger, raw.len()).unwrap_err().code,
            "ZSTD_WINDOW_LIMIT"
        );
        assert_eq!(decode(Codec::Zstd, &e.payload, raw.len()).unwrap(), raw);
    }
    #[test]
    fn frames_are_independent_bounded_and_strict() {
        for (codec, level) in [
            (Codec::Raw, 0),
            (Codec::Zstd, 3),
            (Codec::Xz, 3),
            (Codec::Gzip, 6),
        ] {
            let raw = (0..65536).map(|i| (i % 31) as u8).collect::<Vec<_>>();
            let e = encode(raw.clone(), codec, level).unwrap();
            assert_eq!(decode(e.codec, &e.payload, raw.len()).unwrap(), raw);
            assert!(decode(e.codec, &e.payload, raw.len() - 1).is_err());
            if e.codec != Codec::Raw {
                let mut p = e.payload.clone();
                p.push(0);
                assert!(decode(e.codec, &p, raw.len()).is_err());
                assert!(decode(e.codec, &e.payload[..e.payload.len() - 1], raw.len()).is_err());
            }
            let z = encode(vec![0; 100], codec, level).unwrap();
            assert_eq!(z.codec, Codec::Zero);
        }
    }
}
