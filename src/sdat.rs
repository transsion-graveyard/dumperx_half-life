//! Native `sdat2img` implementation for A-only block-based OTA images.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

const BLOCK_SIZE: u64 = 4096;

#[derive(Debug)]
struct Range {
    begin: u64,
    end: u64,
}

#[derive(Debug)]
struct Command {
    name: String,
    ranges: Vec<Range>,
}

fn parse_rangeset(text: &str) -> Result<Vec<Range>> {
    let nums: Vec<i64> = text
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.trim()
                .parse::<i64>()
                .with_context(|| format!("invalid range integer '{s}'"))
        })
        .collect::<Result<Vec<_>>>()?;
    // The first number is the count of following integers (two per
    // range), e.g. "4,0,2,5,7" describes ranges 0..2 and 5..7.
    if nums.is_empty()
        || nums.len() != nums[0] as usize + 1
        || nums[0] < 0
        || nums[0] % 2 != 0
    {
        bail!("invalid range set '{text}'");
    }
    let mut ranges = Vec::new();
    for pair in nums[1..].chunks(2) {
        if pair[0] < 0 || pair[1] < pair[0] {
            bail!("invalid range {}-{}", pair[0], pair[1]);
        }
        ranges.push(Range {
            begin: pair[0] as u64,
            end: pair[1] as u64,
        });
    }
    Ok(ranges)
}

pub fn parse_transfer_list(path: &Path) -> Result<(u32, u64, Vec<Command>)> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut lines = BufReader::new(file).lines();
    let version: u32 = lines
        .next()
        .transpose()?
        .context("empty transfer list")?
        .trim()
        .parse()
        .context("invalid transfer list version")?;
    let new_blocks: u64 = lines
        .next()
        .transpose()?
        .context("transfer list has no block count")?
        .trim()
        .parse()
        .context("invalid transfer list block count")?;
    if version >= 2 {
        let _ = lines.next();
        let _ = lines.next();
    }
    let mut commands = Vec::new();
    for line in lines {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let name = parts.next().unwrap_or_default();
        match name {
            "new" | "erase" | "zero" => {
                let ranges_text = parts.next().context("command has no range set")?;
                commands.push(Command {
                    name: name.to_string(),
                    ranges: parse_rangeset(ranges_text)?,
                });
            }
            _ if name.chars().next().is_some_and(|c| c.is_ascii_digit()) => continue,
            _ => bail!("invalid transfer list command '{name}'"),
        }
    }
    Ok((version, new_blocks, commands))
}

/// Convert `transfer.list` + `new.dat` into a raw partition image.
pub fn sdat2img(transfer_list: &Path, new_data: &Path, output: &Path) -> Result<u64> {
    let (version, new_blocks, commands) = parse_transfer_list(transfer_list)?;
    let mut data = BufReader::new(
        File::open(new_data).with_context(|| format!("opening {}", new_data.display()))?,
    );
    let mut out = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(output)
        .with_context(|| format!("creating {}", output.display()))?;

    let max_end = commands
        .iter()
        .flat_map(|c| c.ranges.iter())
        .map(|r| r.end)
        .max()
        .unwrap_or(0);
    let expected_size = max_end * BLOCK_SIZE;
    let mut buf = vec![0u8; BLOCK_SIZE as usize];

    for command in &commands {
        match command.name.as_str() {
            "new" => {
                for range in &command.ranges {
                    let block_count = range.end - range.begin;
                    out.seek(SeekFrom::Start(range.begin * BLOCK_SIZE))?;
                    let mut remaining = block_count * BLOCK_SIZE;
                    while remaining > 0 {
                        let want = remaining.min(BLOCK_SIZE) as usize;
                        data.read_exact(&mut buf[..want]).with_context(|| {
                            format!(
                                "unexpected end of {} while writing blocks {}-{}",
                                new_data.display(),
                                range.begin,
                                range.end
                            )
                        })?;
                        out.write_all(&buf[..want])?;
                        remaining -= want as u64;
                    }
                }
            }
            "erase" | "zero" => {
                // Kept for compatibility: erase/zero commands do not consume
                // input data and describe blocks that will be zeroed.
                for range in &command.ranges {
                    let bytes = (range.end - range.begin) * BLOCK_SIZE;
                    out.seek(SeekFrom::Start(range.begin * BLOCK_SIZE))?;
                    let zeros = vec![0u8; BLOCK_SIZE as usize];
                    let mut remaining = bytes;
                    while remaining > 0 {
                        let want = remaining.min(BLOCK_SIZE) as usize;
                        out.write_all(&zeros[..want])?;
                        remaining -= want as u64;
                    }
                }
            }
            _ => {}
        }
    }

    let mut len = out.seek(SeekFrom::End(0))?;
    if len < expected_size {
        out.set_len(expected_size)?;
        len = expected_size;
    }
    out.flush()?;
    println!(
        "sdat2img: version {version}, {new_blocks} new blocks, {} bytes",
        len
    );
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rangeset() {
        let ranges = parse_rangeset("4,0,2,5,7").unwrap();
        assert_eq!(ranges.len(), 2);
        assert_eq!((ranges[0].begin, ranges[0].end), (0, 2));
        assert_eq!((ranges[1].begin, ranges[1].end), (5, 7));
    }

    #[test]
    fn converts_sdat_pair() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-sdat");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let transfer = dir.join("system.transfer.list");
        let new_data = dir.join("system.new.dat");
        std::fs::write(&transfer, "4\n2\n0\n0\nnew 2,0,2\n").unwrap();
        let mut raw = vec![0u8; 4096 * 2];
        raw[..4].copy_from_slice(b"ABCD");
        std::fs::write(&new_data, &raw).unwrap();
        let out = dir.join("system.img");
        sdat2img(&transfer, &new_data, &out).unwrap();
        let data = std::fs::read(&out).unwrap();
        assert_eq!(data.len(), 4096 * 2);
        assert_eq!(&data[..4], b"ABCD");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
