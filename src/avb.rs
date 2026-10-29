//! AVB vbmeta parsing and human-readable `avb-info.txt` generation.
//!
//! Implements the AOSP `avbtool info_image` output for vbmeta images,
//! including chained-partition, hash, hashtree, property and kernel-cmdline
//! descriptors.  No external avbtool/Python dependency is needed.

use crate::util;
use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};
use std::fs;
use std::path::{Path, PathBuf};

const VBETA_MAGIC: &[u8; 4] = b"AVB0";
const FOOTER_MAGIC: &[u8; 4] = b"AVBf";
const HEADER_SIZE: usize = 256;
const FOOTER_SIZE: usize = 64;

#[derive(Debug)]
struct Header {
    required_major: u32,
    required_minor: u32,
    authentication_data_block_size: u64,
    auxiliary_data_block_size: u64,
    algorithm_type: u32,
    public_key_offset: u64,
    public_key_size: u64,
    descriptors_offset: u64,
    descriptors_size: u64,
    rollback_index: u64,
    flags: u32,
    rollback_index_location: u32,
    release_string: String,
}

#[derive(Debug)]
struct Footer {
    version_major: u32,
    version_minor: u32,
    original_image_size: u64,
    vbmeta_offset: u64,
    vbmeta_size: u64,
}

/// Find all top-level vbmeta images, including slot-suffixed names.
pub fn find_vbmeta_images(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.starts_with("vbmeta") && name.ends_with(".img") {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// Write `avb-info.txt` if at least one valid vbmeta image is present.
pub fn write_info(root: &Path) -> Result<Option<PathBuf>> {
    let files = find_vbmeta_images(root)?;
    if files.is_empty() {
        return Ok(None);
    }
    let mut output = String::new();
    output.push_str("# AVB information extracted by DumprX\n");
    let mut valid = 0usize;
    for file in &files {
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        match image_info(file) {
            Ok(info) => {
                output.push_str(&format!(
                    "\n================================================================\n{name}\n================================================================\n"
                ));
                output.push_str(&info);
                valid += 1;
            }
            Err(e) => {
                util::warn(format!("avb: {}: {e:#}", file.display()));
            }
        }
    }
    if valid == 0 {
        return Ok(None);
    }
    let path = root.join("avb-info.txt");
    fs::write(&path, output)?;
    util::info(format!(
        "wrote {} ({valid} vbmeta image(s))",
        path.display()
    ));
    Ok(Some(path))
}

fn image_info(path: &Path) -> Result<String> {
    let mut data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    // Slot images produced by some vendors are sparse.
    if data.len() >= 4 && u32::from_le_bytes(data[..4].try_into().unwrap()) == 0xed26_ff3a {
        let tmp = path.with_extension("avb-unsparse");
        crate::sparse::unsparse(path, &tmp)?;
        data = fs::read(&tmp)?;
        let _ = fs::remove_file(&tmp);
    }
    let image_size = data.len() as u64;
    let footer = parse_footer(&data);
    let vbmeta_offset = footer
        .as_ref()
        .map(|f| f.vbmeta_offset as usize)
        .unwrap_or(0);
    if data.len() < vbmeta_offset + HEADER_SIZE {
        bail!("shorter than a vbmeta header");
    }
    let header = parse_header(&data[vbmeta_offset..])?;
    let auth_start = vbmeta_offset + HEADER_SIZE;
    let auth_end = auth_start
        .checked_add(header.authentication_data_block_size as usize)
        .context("authentication block overflow")?;
    let aux_end = auth_end
        .checked_add(header.auxiliary_data_block_size as usize)
        .context("auxiliary block overflow")?;
    if aux_end > data.len() {
        bail!("authentication/auxiliary blocks extend past end of image");
    }
    let aux_start = auth_end;

    let mut out = String::new();
    if let Some(footer) = &footer {
        out.push_str(&format!(
            "Footer version:           {}.{}\n",
            footer.version_major, footer.version_minor
        ));
        out.push_str(&format!("Image size:               {image_size} bytes\n"));
        out.push_str(&format!(
            "Original image size:      {} bytes\n",
            footer.original_image_size
        ));
        out.push_str(&format!(
            "VBMeta offset:            {}\n",
            footer.vbmeta_offset
        ));
        out.push_str(&format!(
            "VBMeta size:              {} bytes\n--\n",
            footer.vbmeta_size
        ));
    }

    out.push_str(&format!(
        "Minimum libavb version:   {}.{}\n",
        header.required_major, header.required_minor
    ));
    out.push_str(&format!("Header Block:             {HEADER_SIZE} bytes\n"));
    out.push_str(&format!(
        "Authentication Block:     {} bytes\n",
        header.authentication_data_block_size
    ));
    out.push_str(&format!(
        "Auxiliary Block:          {} bytes\n",
        header.auxiliary_data_block_size
    ));
    let key_start = aux_start
        .checked_add(header.public_key_offset as usize)
        .context("public key offset overflow")?;
    let key_end = key_start
        .checked_add(header.public_key_size as usize)
        .context("public key size overflow")?;
    if key_end > data.len() {
        bail!("public key extends past end of image");
    }
    if key_end > key_start {
        out.push_str(&format!(
            "Public key (sha1):        {}\n",
            sha1_hex(&data[key_start..key_end])
        ));
    }
    out.push_str(&format!(
        "Algorithm:                {}\n",
        algorithm_name(header.algorithm_type)
    ));
    out.push_str(&format!(
        "Rollback Index:           {}\n",
        header.rollback_index
    ));
    out.push_str(&format!("Flags:                    {}\n", header.flags));
    out.push_str(&format!(
        "Rollback Index Location:  {}\n",
        header.rollback_index_location
    ));
    out.push_str(&format!(
        "Release String:           '{}'\n",
        header.release_string
    ));

    let desc_start = aux_start
        .checked_add(header.descriptors_offset as usize)
        .context("descriptor offset overflow")?;
    let desc_end = desc_start
        .checked_add(header.descriptors_size as usize)
        .context("descriptor size overflow")?;
    if desc_end > data.len() {
        bail!("descriptor block extends past end of image");
    }
    out.push_str("Descriptors:\n");
    let descriptors = &data[desc_start..desc_end];
    let mut pos = 0usize;
    let mut count = 0usize;
    while pos + 16 <= descriptors.len() {
        let tag = be_u64(descriptors, pos)?;
        let nbf = be_u64(descriptors, pos + 8)? as usize;
        let end = pos
            .checked_add(16 + nbf)
            .context("descriptor length overflow")?;
        if end > descriptors.len() {
            bail!("descriptor at {pos} extends past descriptor block");
        }
        let body = &descriptors[pos + 16..end];
        print_descriptor(&mut out, tag, body)?;
        count += 1;
        pos = end;
    }
    if count == 0 {
        out.push_str("    (none)\n");
    }
    Ok(out)
}

fn print_descriptor(out: &mut String, tag: u64, body: &[u8]) -> Result<()> {
    match tag {
        0 => {
            // Property descriptor: key_size u64, value_size u64.
            let key_size = be_u64(body, 0)? as usize;
            let value_size = be_u64(body, 8)? as usize;
            let key_start = 16;
            let key_end = key_start + key_size;
            let value_start = key_end + 1;
            let value_end = value_start + value_size;
            if value_end > body.len() {
                bail!("property descriptor is truncated");
            }
            let key = String::from_utf8_lossy(&body[key_start..key_end]);
            let value = &body[value_start..value_end];
            if value.len() < 256 {
                out.push_str(&format!(
                    "    Prop: {} -> '{}'\n",
                    key,
                    String::from_utf8_lossy(value)
                ));
            } else {
                out.push_str(&format!("    Prop: {} -> ({} bytes)\n", key, value.len()));
            }
        }
        1 => {
            // Hashtree descriptor.
            let dm_verity = be_u32(body, 0)?;
            let image_size = be_u64(body, 4)?;
            let tree_offset = be_u64(body, 12)?;
            let tree_size = be_u64(body, 20)?;
            let data_block_size = be_u32(body, 28)?;
            let hash_block_size = be_u32(body, 32)?;
            let fec_num_roots = be_u32(body, 36)?;
            let fec_offset = be_u64(body, 40)?;
            let fec_size = be_u64(body, 48)?;
            let hash_algorithm = cstr(&body[56..88]);
            let name_len = be_u32(body, 88)? as usize;
            let salt_len = be_u32(body, 92)? as usize;
            let digest_len = be_u32(body, 96)? as usize;
            let flags = be_u32(body, 100)?;
            let data_start = 164;
            let name_start = data_start;
            let salt_start = name_start + name_len;
            let digest_start = salt_start + salt_len;
            let data_end = digest_start + digest_len;
            if data_end > body.len() {
                bail!("hashtree descriptor is truncated");
            }
            out.push_str("    Hashtree descriptor:\n");
            out.push_str(&format!("      Version of dm-verity:  {dm_verity}\n"));
            out.push_str(&format!(
                "      Image Size:            {image_size} bytes\n"
            ));
            out.push_str(&format!("      Tree Offset:           {tree_offset}\n"));
            out.push_str(&format!("      Tree Size:             {tree_size} bytes\n"));
            out.push_str(&format!(
                "      Data Block Size:       {data_block_size} bytes\n"
            ));
            out.push_str(&format!(
                "      Hash Block Size:       {hash_block_size} bytes\n"
            ));
            out.push_str(&format!("      FEC num roots:         {fec_num_roots}\n"));
            out.push_str(&format!("      FEC offset:            {fec_offset}\n"));
            out.push_str(&format!("      FEC size:              {fec_size} bytes\n"));
            out.push_str(&format!("      Hash Algorithm:        {hash_algorithm}\n"));
            out.push_str(&format!(
                "      Partition Name:        {}\n",
                String::from_utf8_lossy(&body[name_start..salt_start])
            ));
            out.push_str(&format!(
                "      Salt:                  {}\n",
                hex(&body[salt_start..digest_start])
            ));
            out.push_str(&format!(
                "      Root Digest:           {}\n",
                hex(&body[digest_start..data_end])
            ));
            out.push_str(&format!("      Flags:                 {flags}\n"));
        }
        2 => {
            // Hash descriptor.
            let image_size = be_u64(body, 0)?;
            let hash_algorithm = cstr(&body[8..40]);
            let name_len = be_u32(body, 40)? as usize;
            let salt_len = be_u32(body, 44)? as usize;
            let digest_len = be_u32(body, 48)? as usize;
            let flags = be_u32(body, 52)?;
            let data_start = 116;
            let name_start = data_start;
            let salt_start = name_start + name_len;
            let digest_start = salt_start + salt_len;
            let data_end = digest_start + digest_len;
            if data_end > body.len() {
                bail!("hash descriptor is truncated");
            }
            out.push_str("    Hash descriptor:\n");
            out.push_str(&format!(
                "      Image Size:            {image_size} bytes\n"
            ));
            out.push_str(&format!("      Hash Algorithm:        {hash_algorithm}\n"));
            out.push_str(&format!(
                "      Partition Name:        {}\n",
                String::from_utf8_lossy(&body[name_start..salt_start])
            ));
            out.push_str(&format!(
                "      Salt:                  {}\n",
                hex(&body[salt_start..digest_start])
            ));
            out.push_str(&format!(
                "      Digest:                {}\n",
                hex(&body[digest_start..data_end])
            ));
            out.push_str(&format!("      Flags:                 {flags}\n"));
        }
        3 => {
            // Kernel command-line descriptor.
            let flags = be_u32(body, 0)?;
            let cmdline_len = be_u32(body, 4)? as usize;
            if 8 + cmdline_len > body.len() {
                bail!("kernel cmdline descriptor is truncated");
            }
            out.push_str("    Kernel Cmdline descriptor:\n");
            out.push_str(&format!("      Flags:                 {flags}\n"));
            out.push_str(&format!(
                "      Kernel Cmdline:        '{}'\n",
                String::from_utf8_lossy(&body[8..8 + cmdline_len])
            ));
        }
        4 => {
            // Chained partition descriptor.
            let rollback_index_location = be_u32(body, 0)?;
            let name_len = be_u32(body, 4)? as usize;
            let key_len = be_u32(body, 8)? as usize;
            let flags = be_u32(body, 12)?;
            let data_start = 76;
            let name_start = data_start;
            let key_start = name_start + name_len;
            let key_end = key_start + key_len;
            if key_end > body.len() {
                bail!("chain partition descriptor is truncated");
            }
            out.push_str("    Chain Partition descriptor:\n");
            out.push_str(&format!(
                "      Partition Name:          {}\n",
                String::from_utf8_lossy(&body[name_start..key_start])
            ));
            out.push_str(&format!(
                "      Rollback Index Location: {rollback_index_location}\n"
            ));
            out.push_str(&format!(
                "      Public key (sha1):       {}\n",
                sha1_hex(&body[key_start..key_end])
            ));
            out.push_str(&format!("      Flags:                   {flags}\n"));
        }
        _ => {
            out.push_str("    Unknown descriptor:\n");
            out.push_str(&format!("      Tag:  {tag}\n"));
            out.push_str(&format!("      Data: {} bytes\n", body.len()));
        }
    }
    Ok(())
}

fn parse_header(data: &[u8]) -> Result<Header> {
    if data.len() < HEADER_SIZE {
        bail!("vbmeta header is truncated");
    }
    if &data[0..4] != VBETA_MAGIC {
        bail!("not a vbmeta image (magic {:?})", &data[0..4]);
    }
    Ok(Header {
        required_major: be_u32(data, 4)?,
        required_minor: be_u32(data, 8)?,
        authentication_data_block_size: be_u64(data, 12)?,
        auxiliary_data_block_size: be_u64(data, 20)?,
        algorithm_type: be_u32(data, 28)?,
        public_key_offset: be_u64(data, 64)?,
        public_key_size: be_u64(data, 72)?,
        descriptors_offset: be_u64(data, 96)?,
        descriptors_size: be_u64(data, 104)?,
        rollback_index: be_u64(data, 112)?,
        flags: be_u32(data, 120)?,
        rollback_index_location: be_u32(data, 124)?,
        release_string: cstr(&data[128..176]).trim().to_string(),
    })
}

fn parse_footer(data: &[u8]) -> Option<Footer> {
    if data.len() < FOOTER_SIZE {
        return None;
    }
    let footer = &data[data.len() - FOOTER_SIZE..];
    if &footer[0..4] != FOOTER_MAGIC {
        return None;
    }
    Some(Footer {
        version_major: be_u32(footer, 4).ok()?,
        version_minor: be_u32(footer, 8).ok()?,
        original_image_size: be_u64(footer, 12).ok()?,
        vbmeta_offset: be_u64(footer, 20).ok()?,
        vbmeta_size: be_u64(footer, 28).ok()?,
    })
}

fn algorithm_name(algorithm_type: u32) -> &'static str {
    match algorithm_type {
        0 => "NONE",
        1 => "SHA256_RSA2048",
        2 => "SHA256_RSA4096",
        3 => "SHA256_RSA8192",
        4 => "SHA512_RSA2048",
        5 => "SHA512_RSA4096",
        6 => "SHA512_RSA8192",
        _ => "UNKNOWN",
    }
}

fn be_u32(data: &[u8], offset: usize) -> Result<u32> {
    let end = offset.checked_add(4).context("offset overflow")?;
    if end > data.len() {
        bail!("field at {offset} is out of bounds");
    }
    Ok(u32::from_be_bytes(data[offset..end].try_into().unwrap()))
}

fn be_u64(data: &[u8], offset: usize) -> Result<u64> {
    let end = offset.checked_add(8).context("offset overflow")?;
    if end > data.len() {
        bail!("field at {offset} is out of bounds");
    }
    Ok(u64::from_be_bytes(data[offset..end].try_into().unwrap()))
}

fn cstr(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn sha1_hex(data: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

pub fn is_vbmeta_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("vbmeta") && lower.ends_with(".img")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u32(buf: &mut Vec<u8>, value: u32) {
        buf.extend_from_slice(&value.to_be_bytes());
    }
    fn put_u64(buf: &mut Vec<u8>, value: u64) {
        buf.extend_from_slice(&value.to_be_bytes());
    }

    #[test]
    fn parses_property_and_cmdline_descriptors() {
        let mut descs = Vec::new();
        // Property descriptor: tag 0, key "foo", value "bar".
        let key = b"foo";
        let value = b"bar";
        let mut body = Vec::new();
        put_u64(&mut body, key.len() as u64);
        put_u64(&mut body, value.len() as u64);
        body.extend_from_slice(key);
        body.push(0);
        body.extend_from_slice(value);
        body.push(0);
        let nbf = (body.len() + 7) / 8 * 8;
        descs.extend_from_slice(&0u64.to_be_bytes());
        descs.extend_from_slice(&(nbf as u64).to_be_bytes());
        descs.extend_from_slice(&body);
        descs.resize(16 + nbf, 0);

        // Kernel cmdline descriptor.
        let cmd = b"console=tty0";
        let mut body = Vec::new();
        put_u32(&mut body, 0);
        put_u32(&mut body, cmd.len() as u32);
        body.extend_from_slice(cmd);
        let nbf = (body.len() + 7) / 8 * 8;
        descs.extend_from_slice(&3u64.to_be_bytes());
        descs.extend_from_slice(&(nbf as u64).to_be_bytes());
        descs.extend_from_slice(&body);
        descs.resize(descs.len() + (nbf - body.len()), 0);

        let mut header = Vec::new();
        header.extend_from_slice(VBETA_MAGIC);
        put_u32(&mut header, 1);
        put_u32(&mut header, 0);
        put_u64(&mut header, 0); // auth block
        put_u64(&mut header, descs.len() as u64); // aux block
        put_u32(&mut header, 0); // algorithm NONE
        put_u64(&mut header, 0);
        put_u64(&mut header, 0);
        put_u64(&mut header, 0);
        put_u64(&mut header, 0);
        put_u64(&mut header, 0); // public key offset
        put_u64(&mut header, 0); // public key size
        put_u64(&mut header, 0); // public key metadata offset
        put_u64(&mut header, 0); // public key metadata size
        put_u64(&mut header, 0); // descriptors offset
        put_u64(&mut header, descs.len() as u64);
        put_u64(&mut header, 6);
        put_u32(&mut header, 0);
        put_u32(&mut header, 0);
        header.extend_from_slice(b"avbtool 1.3.0\0");
        header.resize(HEADER_SIZE, 0);
        header.extend_from_slice(&descs);

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-avb");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vbmeta.img"), &header).unwrap();
        write_info(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join("avb-info.txt")).unwrap();
        assert!(text.contains("Algorithm:                NONE"));
        assert!(text.contains("Prop: foo -> 'bar'"));
        assert!(text.contains("Kernel Cmdline:        'console=tty0'"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
