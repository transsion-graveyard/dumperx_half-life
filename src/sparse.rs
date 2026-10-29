//! Android sparse image support (`simg2img` equivalent).

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const SPARSE_MAGIC: u32 = 0xed26_ff3a;

const CHUNK_TYPE_RAW: u16 = 0xcac1;
const CHUNK_TYPE_FILL: u16 = 0xcac2;
const CHUNK_TYPE_DONT_CARE: u16 = 0xcac3;
const CHUNK_TYPE_CRC32: u16 = 0xcac4;

#[derive(Debug, Clone)]
pub struct SparseHeader {
    pub major: u16,
    pub minor: u16,
    pub file_hdr_sz: u16,
    pub chunk_hdr_sz: u16,
    pub blk_sz: u32,
    pub total_blks: u32,
    pub total_chunks: u32,
    pub image_checksum: u32,
}

pub fn is_sparse(data: &[u8]) -> bool {
    data.len() >= 4 && u32::from_le_bytes(data[..4].try_into().unwrap()) == SPARSE_MAGIC
}

pub fn read_header(reader: &mut impl Read) -> Result<Option<SparseHeader>> {
    let mut magic = [0u8; 4];
    if reader.read(&mut magic)? < 4 {
        return Ok(None);
    }
    if u32::from_le_bytes(magic) != SPARSE_MAGIC {
        return Ok(None);
    }
    let mut buf = [0u8; 24];
    reader.read_exact(&mut buf)?;
    Ok(Some(SparseHeader {
        major: u16::from_le_bytes(buf[0..2].try_into().unwrap()),
        minor: u16::from_le_bytes(buf[2..4].try_into().unwrap()),
        file_hdr_sz: u16::from_le_bytes(buf[4..6].try_into().unwrap()),
        chunk_hdr_sz: u16::from_le_bytes(buf[6..8].try_into().unwrap()),
        blk_sz: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
        total_blks: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
        total_chunks: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
        image_checksum: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
    }))
}

/// Convert a sparse image to a raw image.  Returns `Ok(false)` if the input
/// was not sparse (nothing is written in that case).
pub fn unsparse(src: &Path, dst: &Path) -> Result<bool> {
    let mut input = BufReader::new(
        File::open(src).with_context(|| format!("opening {}", src.display()))?,
    );
    let header = match read_header(&mut input)? {
        Some(h) => h,
        None => return Ok(false),
    };
    if header.major != 1 {
        bail!("unsupported sparse image major version {}", header.major);
    }
    if header.file_hdr_sz < 28 {
        bail!("invalid sparse file header size {}", header.file_hdr_sz);
    }
    // Skip any unknown part of the file header.
    let already = 28u64;
    if header.file_hdr_sz as u64 > already {
        std::io::copy(
            &mut input.by_ref().take(header.file_hdr_sz as u64 - already),
            &mut std::io::sink(),
        )?;
    }

    let out = File::create(dst).with_context(|| format!("creating {}", dst.display()))?;
    let mut out = BufWriter::new(out);
    let mut written_blocks: u64 = 0;
    let block = header.blk_sz as usize;
    if block == 0 {
        bail!("sparse image has zero block size");
    }

    for chunk_idx in 0..header.total_chunks {
        let mut hdr = vec![0u8; header.chunk_hdr_sz as usize];
        input
            .read_exact(&mut hdr)
            .with_context(|| format!("reading sparse chunk {chunk_idx}"))?;
        if header.chunk_hdr_sz < 12 {
            bail!("invalid sparse chunk header size {}", header.chunk_hdr_sz);
        }
        let chunk_type = u16::from_le_bytes(hdr[0..2].try_into().unwrap());
        let chunk_blocks = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as u64;
        let total_sz = u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as u64;
        let data_sz = total_sz
            .checked_sub(header.chunk_hdr_sz as u64)
            .context("sparse chunk total size smaller than header")?;
        let out_bytes = chunk_blocks * block as u64;

        match chunk_type {
            CHUNK_TYPE_RAW => {
                if data_sz != out_bytes {
                    bail!(
                        "sparse raw chunk {chunk_idx} has {data_sz} data bytes for {out_bytes} output bytes"
                    );
                }
                std::io::copy(&mut input.by_ref().take(data_sz), &mut out)?;
            }
            CHUNK_TYPE_FILL => {
                if data_sz != 4 {
                    bail!("sparse fill chunk {chunk_idx} has {data_sz} data bytes, expected 4");
                }
                let mut fill = [0u8; 4];
                input.read_exact(&mut fill)?;
                let buf = vec![0u8; block];
                let mut buf = buf;
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = fill[i % 4];
                }
                for _ in 0..chunk_blocks {
                    out.write_all(&buf)?;
                }
            }
            CHUNK_TYPE_DONT_CARE => {
                if data_sz > 0 {
                    std::io::copy(
                        &mut input.by_ref().take(data_sz),
                        &mut std::io::sink(),
                    )?;
                }
                out.seek(SeekFrom::Current(out_bytes as i64))?;
            }
            CHUNK_TYPE_CRC32 => {
                let mut crc = [0u8; 4];
                if data_sz >= 4 {
                    input.read_exact(&mut crc)?;
                    std::io::copy(
                        &mut input.by_ref().take(data_sz - 4),
                        &mut std::io::sink(),
                    )?;
                }
            }
            other => bail!("unknown sparse chunk type {other:#06x} at chunk {chunk_idx}"),
        }
        written_blocks += chunk_blocks;
    }

    if written_blocks != header.total_blks as u64 {
        bail!(
            "sparse image wrote {written_blocks} blocks, header says {}",
            header.total_blks
        );
    }
    let expected = header.total_blks as u64 * header.blk_sz as u64;
    out.flush()?;
    let actual = out.get_ref().metadata()?.len();
    if actual < expected {
        out.get_ref().set_len(expected)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn build_sparse() -> Vec<u8> {
        let block = 8u32;
        let mut buf = Vec::new();
        buf.extend_from_slice(&SPARSE_MAGIC.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&28u16.to_le_bytes());
        buf.extend_from_slice(&12u16.to_le_bytes());
        buf.extend_from_slice(&block.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes()); // total blocks
        buf.extend_from_slice(&3u32.to_le_bytes()); // chunks
        buf.extend_from_slice(&0u32.to_le_bytes()); // checksum
        // raw chunk 1 block
        buf.extend_from_slice(&CHUNK_TYPE_RAW.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&(12u32 + 8).to_le_bytes());
        buf.extend_from_slice(b"ABCDEFGH");
        // fill chunk 1 block
        buf.extend_from_slice(&CHUNK_TYPE_FILL.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&0x11223344u32.to_le_bytes());
        // dont care 1 block
        buf.extend_from_slice(&CHUNK_TYPE_DONT_CARE.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&12u32.to_le_bytes());
        buf
    }

    #[test]
    fn unsparses_all_chunk_types() {
        let dir = std::env::temp_dir().join("dumprx-sparse-test");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.sparse");
        let dst = dir.join("out.raw");
        let mut f = File::create(&src).unwrap();
        f.write_all(&build_sparse()).unwrap();
        drop(f);
        assert!(unsparse(&src, &dst).unwrap());
        let data = std::fs::read(&dst).unwrap();
        assert_eq!(data.len(), 24);
        assert_eq!(&data[..8], b"ABCDEFGH");
        assert_eq!(&data[8..16], &[0x44, 0x33, 0x22, 0x11, 0x44, 0x33, 0x22, 0x11]);
        assert_eq!(&data[16..24], &[0u8; 8]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
