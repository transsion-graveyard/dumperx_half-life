//! Android OTA `payload.bin` extraction.
//!
//! Implements the update_engine payload container and the subset of the
//! protobuf manifest needed for full (non-incremental) OTAs.  REPLACE,
//! REPLACE_BZ, REPLACE_XZ, REPLACE_ZSTD, ZERO and DISCARD operations are
//! supported.  Incremental SOURCE_COPY/BSDIFF operations require a base
//! image and are reported as unsupported.

use crate::util;
use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const MAGIC: &[u8; 4] = b"CrAU";

#[derive(Debug, Clone)]
pub struct PayloadPartition {
    pub name: String,
    pub new_size: Option<u64>,
    pub operations: Vec<InstallOperation>,
}

#[derive(Debug, Clone)]
pub struct InstallOperation {
    pub op_type: u64,
    pub data_offset: u64,
    pub data_length: u64,
    pub dst_extents: Vec<Extent>,
}

#[derive(Debug, Clone, Copy)]
pub struct Extent {
    pub start_block: u64,
    pub num_blocks: u64,
}

pub fn is_payload(head: &[u8]) -> bool {
    head.len() >= 4 && &head[..4] == MAGIC
}

/// Extract a full OTA payload.  Returns the extracted partition images.
pub fn extract(payload: &Path, out_dir: &Path) -> Result<Vec<(String, u64)>> {
    util::ensure_dir(out_dir)?;
    let mut file = File::open(payload).with_context(|| format!("opening {}", payload.display()))?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header)?;
    if &header[..4] != MAGIC {
        bail!("not an OTA payload (bad magic)");
    }
    let version = u64::from_be_bytes(header[4..12].try_into().unwrap());
    let manifest_size = u64::from_be_bytes(header[12..20].try_into().unwrap());
    let metadata_signature_size = if version >= 2 {
        u32::from_be_bytes(header[20..24].try_into().unwrap()) as u64
    } else {
        0
    };
    let header_size = if version >= 2 { 24u64 } else { 20u64 };

    if manifest_size == 0 || manifest_size > 512 * 1024 * 1024 {
        bail!("implausible payload manifest size {manifest_size}");
    }
    let mut manifest = vec![0u8; manifest_size as usize];
    file.read_exact(&mut manifest)?;
    let data_offset = header_size + manifest_size + metadata_signature_size;

    let (block_size, partitions) = parse_manifest(&manifest)?;
    util::info(format!(
        "payload v{version}: {} partition(s), block size {block_size}, data offset {data_offset}",
        partitions.len()
    ));

    let mut results = Vec::new();
    for partition in partitions {
        if partition.name.is_empty() {
            continue;
        }
        let image = out_dir.join(format!("{}.img", partition.name));
        let mut sink = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&image)
            .with_context(|| format!("creating {}", image.display()))?;
        let mut size = partition.new_size.unwrap_or(0);
        for op in &partition.operations {
            for extent in &op.dst_extents {
                size = size.max((extent.start_block + extent.num_blocks) * block_size);
            }
        }
        if size == 0 {
            // Nothing to write (metadata-only partition entry).
            let _ = std::fs::remove_file(&image);
            continue;
        }
        sink.set_len(size)?;

        for (index, op) in partition.operations.iter().enumerate() {
            let data = match op.op_type {
                0 | 1 | 8 | 14 => {
                    if op.data_length == 0 {
                        Vec::new()
                    } else {
                        file.seek(SeekFrom::Start(data_offset + op.data_offset))?;
                        let mut buf = vec![0u8; op.data_length as usize];
                        file.read_exact(&mut buf).with_context(|| {
                            format!(
                                "reading operation {index} for {} (offset {}, length {})",
                                partition.name, op.data_offset, op.data_length
                            )
                        })?;
                        match op.op_type {
                            0 => buf,
                            1 => run_filter("bzip2", &["-dc"], &buf)?,
                            8 => run_filter("xz", &["-dc"], &buf)?,
                            14 => run_filter("zstd", &["-dc"], &buf)?,
                            _ => unreachable!(),
                        }
                    }
                }
                6 | 7 => Vec::new(), // ZERO / DISCARD
                other => {
                    util::warn(format!(
                        "{}: unsupported payload operation type {other} at op {index}; skipping (incremental OTA?)",
                        partition.name
                    ));
                    continue;
                }
            };

            if op.op_type == 6 || op.op_type == 7 {
                continue;
            }
            write_extents(&mut sink, block_size, &op.dst_extents, &data)
                .with_context(|| format!("writing operation {index} for {}", partition.name))?;
        }
        sink.flush()?;
        results.push((partition.name.clone(), size));
        util::info(format!(
            "  {} -> {} ({})",
            partition.name,
            image.display(),
            util::human_size(size)
        ));
    }
    Ok(results)
}

fn write_extents(sink: &mut File, block_size: u64, extents: &[Extent], data: &[u8]) -> Result<()> {
    let mut cursor = 0usize;
    for extent in extents {
        if cursor >= data.len() {
            break;
        }
        let bytes = (extent.num_blocks * block_size) as usize;
        let take = bytes.min(data.len() - cursor);
        sink.seek(SeekFrom::Start(extent.start_block * block_size))?;
        sink.write_all(&data[cursor..cursor + take])?;
        cursor += take;
        // A short final chunk is valid; the rest of the block stays zero.
    }
    Ok(())
}

fn run_filter(program: &str, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let tool = util::find_tool(program)
        .with_context(|| format!("payload decompressor '{program}' not found"))?;
    let mut child = Command::new(&tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {}", tool.display()))?;
    let mut stdin = child
        .stdin
        .take()
        .context("decompressor stdin unavailable")?;
    let data = input.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        stdin.write_all(&data)?;
        Ok(())
    });
    let output = child.wait_with_output()?;
    let _ = writer.join();
    if !output.status.success() {
        bail!(
            "{} failed: {}",
            tool.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn parse_manifest(data: &[u8]) -> Result<(u64, Vec<PayloadPartition>)> {
    let mut block_size = 4096u64;
    let mut partitions = Vec::new();
    let mut reader = ProtoReader::new(data);
    while let Some((field, wire, value)) = reader.next_field()? {
        match (field, wire) {
            (3, 0) => block_size = value.as_varint().unwrap_or(block_size),
            (13, 2) => {
                partitions.push(parse_partition(value.as_bytes().unwrap_or(&[]))?);
            }
            _ => {}
        }
    }
    Ok((block_size, partitions))
}

fn parse_partition(data: &[u8]) -> Result<PayloadPartition> {
    let mut partition = PayloadPartition {
        name: String::new(),
        new_size: None,
        operations: Vec::new(),
    };
    let mut reader = ProtoReader::new(data);
    while let Some((field, wire, value)) = reader.next_field()? {
        match (field, wire) {
            (1, 2) => {
                partition.name =
                    String::from_utf8_lossy(value.as_bytes().unwrap_or(&[])).into_owned()
            }
            (2, 2) => partition
                .operations
                .push(parse_operation(value.as_bytes().unwrap_or(&[]))?),
            (7, 2) => partition.new_size = parse_partition_info(value.as_bytes().unwrap_or(&[]))?,
            _ => {}
        }
    }
    Ok(partition)
}

fn parse_partition_info(data: &[u8]) -> Result<Option<u64>> {
    let mut size = None;
    let mut reader = ProtoReader::new(data);
    while let Some((field, wire, value)) = reader.next_field()? {
        if field == 1 && wire == 0 {
            size = value.as_varint();
        }
    }
    Ok(size)
}

fn parse_operation(data: &[u8]) -> Result<InstallOperation> {
    let mut op = InstallOperation {
        op_type: 0,
        data_offset: 0,
        data_length: 0,
        dst_extents: Vec::new(),
    };
    let mut reader = ProtoReader::new(data);
    while let Some((field, wire, value)) = reader.next_field()? {
        match (field, wire) {
            (1, 0) => op.op_type = value.as_varint().unwrap_or(0),
            (2, 0) => op.data_offset = value.as_varint().unwrap_or(0),
            (3, 0) => op.data_length = value.as_varint().unwrap_or(0),
            (6, 2) => op
                .dst_extents
                .push(parse_extent(value.as_bytes().unwrap_or(&[]))?),
            _ => {}
        }
    }
    Ok(op)
}

fn parse_extent(data: &[u8]) -> Result<Extent> {
    let mut extent = Extent {
        start_block: 0,
        num_blocks: 0,
    };
    let mut reader = ProtoReader::new(data);
    while let Some((field, wire, value)) = reader.next_field()? {
        if wire == 0 {
            match field {
                1 => extent.start_block = value.as_varint().unwrap_or(0),
                2 => extent.num_blocks = value.as_varint().unwrap_or(0),
                _ => {}
            }
        }
    }
    Ok(extent)
}

enum ProtoValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
    Fixed64(u64),
}

impl<'a> ProtoValue<'a> {
    fn as_varint(&self) -> Option<u64> {
        match self {
            ProtoValue::Varint(v) => Some(*v),
            _ => None,
        }
    }

    fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            ProtoValue::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

struct ProtoReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ProtoReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        ProtoReader { data, pos: 0 }
    }

    fn next_field(&mut self) -> Result<Option<(u64, u8, ProtoValue<'a>)>> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let key = self.read_varint()?;
        let field = key >> 3;
        let wire = (key & 7) as u8;
        let value = match wire {
            0 => ProtoValue::Varint(self.read_varint()?),
            1 => {
                let v = self.read_fixed64()?;
                ProtoValue::Fixed64(v)
            }
            2 => {
                let len = self.read_varint()? as usize;
                if self.pos + len > self.data.len() {
                    bail!("protobuf length-delimited field exceeds manifest");
                }
                let bytes = &self.data[self.pos..self.pos + len];
                self.pos += len;
                ProtoValue::Bytes(bytes)
            }
            5 => {
                let v = self.read_fixed32()?;
                ProtoValue::Fixed32(v)
            }
            other => bail!("unsupported protobuf wire type {other}"),
        };
        Ok(Some((field, wire, value)))
    }

    fn read_varint(&mut self) -> Result<u64> {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            if self.pos >= self.data.len() {
                bail!("truncated protobuf varint");
            }
            let byte = self.data[self.pos];
            self.pos += 1;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                bail!("protobuf varint is too long");
            }
        }
    }

    fn read_fixed32(&mut self) -> Result<u32> {
        if self.pos + 4 > self.data.len() {
            bail!("truncated protobuf fixed32");
        }
        let v = u32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Ok(v)
    }

    fn read_fixed64(&mut self) -> Result<u64> {
        if self.pos + 8 > self.data.len() {
            bail!("truncated protobuf fixed64");
        }
        let v = u64::from_le_bytes(self.data[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        Ok(v)
    }
}

/// Legacy path: run the bundled payload-dumper-go.  Used only if the native
/// parser encounters a manifest shape it does not understand yet.
pub fn extract_legacy(payload: &Path, out_dir: &Path) -> Result<()> {
    util::ensure_dir(out_dir)?;
    let tool = util::require_tool("payload-dumper-go")?;
    let jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .to_string();
    crate::util::Runner::new(false).run(
        &tool,
        &[
            std::ffi::OsStr::new("-c"),
            std::ffi::OsStr::new(&jobs),
            std::ffi::OsStr::new("-o"),
            out_dir.as_os_str(),
            payload.as_os_str(),
        ],
        None,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }

    fn field_bytes(field: u64, data: &[u8], out: &mut Vec<u8>) {
        varint((field << 3) | 2, out);
        varint(data.len() as u64, out);
        out.extend_from_slice(data);
    }

    fn field_varint(field: u64, value: u64, out: &mut Vec<u8>) {
        varint(field << 3, out);
        varint(value, out);
    }

    #[test]
    fn extracts_minimal_payload() {
        let mut extent = Vec::new();
        field_varint(1, 0, &mut extent);
        field_varint(2, 1, &mut extent);
        let mut op = Vec::new();
        field_varint(1, 0, &mut op);
        field_varint(2, 0, &mut op);
        field_varint(3, 4, &mut op);
        field_bytes(6, &extent, &mut op);
        let mut info = Vec::new();
        field_varint(1, 4096, &mut info);
        let mut part = Vec::new();
        field_bytes(1, b"boot", &mut part);
        field_bytes(2, &op, &mut part);
        field_bytes(7, &info, &mut part);
        let mut manifest = Vec::new();
        field_varint(3, 4096, &mut manifest);
        field_bytes(13, &part, &mut manifest);

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-payload");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = dir.join("payload.bin");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&2u64.to_be_bytes());
        bytes.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&manifest);
        bytes.extend_from_slice(b"ABCD");
        std::fs::write(&payload, &bytes).unwrap();

        let out = dir.join("out");
        extract(&payload, &out).unwrap();
        let image = std::fs::read(out.join("boot.img")).unwrap();
        assert_eq!(image.len(), 4096);
        assert_eq!(&image[..4], b"ABCD");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_minimal_manifest() {
        // PartitionUpdate { name: "boot", new_partition_info { size: 4096 },
        //                  operations { type: REPLACE, data_offset: 0,
        //                               data_length: 4,
        //                               dst_extents { start_block: 0, num_blocks: 1 } } }
        let mut extent = Vec::new();
        field_varint(1, 0, &mut extent);
        field_varint(2, 1, &mut extent);
        let mut op = Vec::new();
        field_varint(1, 0, &mut op); // REPLACE
        field_varint(2, 0, &mut op);
        field_varint(3, 4, &mut op);
        field_bytes(6, &extent, &mut op);
        let mut info = Vec::new();
        field_varint(1, 4096, &mut info);
        let mut part = Vec::new();
        field_bytes(1, b"boot", &mut part);
        field_bytes(2, &op, &mut part);
        field_bytes(7, &info, &mut part);
        let mut manifest = Vec::new();
        field_varint(3, 4096, &mut manifest);
        field_bytes(13, &part, &mut manifest);

        let (block_size, partitions) = parse_manifest(&manifest).unwrap();
        assert_eq!(block_size, 4096);
        assert_eq!(partitions.len(), 1);
        assert_eq!(partitions[0].name, "boot");
        assert_eq!(partitions[0].new_size, Some(4096));
        assert_eq!(partitions[0].operations[0].dst_extents[0].num_blocks, 1);
    }
}
