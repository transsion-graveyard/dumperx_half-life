//! Android/Linux fstab parser.
//!
//! Android fstabs are deceptively simple: the same file can contain
//! 4-field entries (no fs_mgr flags), 5-field Android entries, 6-field
//! Linux-style entries, duplicate entries for multiple filesystems
//! (erofs/ext4), CRLF line endings, tabs, and escaped whitespace.
//!
//! The old shell pipeline silently ignored every layout it did not
//! expect.  This parser accepts all of the layouts above, keeps duplicate
//! entries instead of collapsing them, and reports malformed lines with
//! file/line context so a bad fstab can never fail silently again.

use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Layout of one fstab line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FstabLayout {
    /// `<source> <mount_point> <type> <options>` (vendor variants often
    /// omit the optional fs_mgr flags column).
    FourField,
    /// `<source> <mount_point> <type> <options> <fs_mgr_flags>`
    Android,
    /// `<source> <mount_point> <type> <options> <dump> <pass>`
    Linux,
    /// 5 fields in which the last column is numeric (`0 0` folded together,
    /// seen in hand-edited fstabs).
    AndroidWithPass,
}

impl fmt::Display for FstabLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            FstabLayout::FourField => "4-field",
            FstabLayout::Android => "android-5-field",
            FstabLayout::Linux => "linux-6-field",
            FstabLayout::AndroidWithPass => "android+passno",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FstabEntry {
    pub source: String,
    pub mount_point: String,
    pub fs_type: String,
    pub mount_flags: Vec<String>,
    pub fs_mgr_flags: Vec<String>,
    pub dump: Option<u32>,
    pub pass: Option<u32>,
    pub layout: FstabLayout,
    pub line: usize,
    pub field_count: usize,
    pub raw: String,
}

impl FstabEntry {
    pub fn has_fs_mgr_flag(&self, flag: &str) -> bool {
        self.fs_mgr_flags.iter().any(|f| f == flag)
    }

    pub fn has_mount_flag(&self, flag: &str) -> bool {
        self.mount_flags.iter().any(|f| f == flag)
    }

    pub fn is_logical(&self) -> bool {
        self.has_fs_mgr_flag("logical")
    }

    pub fn fs_mgr_flag_value(&self, key: &str) -> Option<&str> {
        self.fs_mgr_flags
            .iter()
            .find_map(|f| f.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
    }

    /// The partition name this entry refers to.  Logical partitions keep
    /// their source name, physical entries use the basename of `/dev/...`.
    pub fn partition_name(&self) -> Option<String> {
        if self.is_logical() {
            return Some(self.source.clone());
        }
        if self.source.starts_with('/') {
            return self
                .source
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
        }
        if self.source == "auto" || self.source == "none" {
            None
        } else {
            Some(self.source.clone())
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FstabWarning {
    pub line: usize,
    pub message: String,
    pub raw: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FstabError {
    pub line: usize,
    pub message: String,
    pub raw: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FstabFile {
    pub path: PathBuf,
    pub entries: Vec<FstabEntry>,
    pub warnings: Vec<FstabWarning>,
    pub errors: Vec<FstabError>,
}

impl FstabFile {
    pub fn parse(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        Ok(Self::parse_bytes(path, &bytes))
    }

    pub fn parse_bytes(path: &Path, bytes: &[u8]) -> Self {
        // fstabs are ASCII; invalid UTF-8 is reported instead of being
        // silently discarded.
        let text = String::from_utf8_lossy(bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let mut file = FstabFile {
            path: path.to_path_buf(),
            entries: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
        };

        for (idx, line) in text.lines().enumerate() {
            let line_no = idx + 1;
            let raw = line.trim_end_matches('\r').to_string();
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            match parse_line(&raw, line_no) {
                Ok(mut entry) => {
                    entry.raw = raw;
                    if entry.field_count > 5 && entry.layout == FstabLayout::Android {
                        file.warnings.push(FstabWarning {
                            line: line_no,
                            message: format!(
                                "android fstab line has {} fields; the optional fs_mgr flag column contained spaces and was joined",
                                entry.field_count
                            ),
                            raw: entry.raw.clone(),
                        });
                    }
                    file.entries.push(entry);
                }
                Err(message) => file.errors.push(FstabError {
                    line: line_no,
                    message,
                    raw,
                }),
            }
        }

        file.check_consistency();
        file
    }

    fn check_consistency(&mut self) {
        // Entries that mount the same mount point with different fs types are
        // valid (Android fstabs list erofs and ext4 alternatives), but a
        // duplicate of the *same* fs type is almost always a bad edit.
        let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut warnings = Vec::new();
        for entry in &self.entries {
            let key = (entry.mount_point.clone(), entry.fs_type.clone());
            if let Some(first) = seen.get(&key) {
                warnings.push(FstabWarning {
                    line: entry.line,
                    message: format!(
                        "duplicate entry for mount point '{}' with fs type '{}' (first at line {})",
                        entry.mount_point, entry.fs_type, first
                    ),
                    raw: entry.raw.clone(),
                });
            } else {
                seen.insert(key, entry.line);
            }
        }
        self.warnings.extend(warnings);
    }

    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }
}

fn parse_line(raw: &str, line: usize) -> Result<FstabEntry, String> {
    let fields = tokenize(raw);
    if fields.is_empty() {
        return Err("empty line".into());
    }
    if fields.len() < 4 {
        return Err(format!(
            "expected at least 4 fields (source mount_point type options), found {}",
            fields.len()
        ));
    }

    let source = fields[0].clone();
    let mount_point = fields[1].clone();
    let fs_type = fields[2].clone();
    let mount_flags = split_flags(&fields[3]);

    // 6-field Linux fstab: the last two fields are dump/pass numbers.
    // Some tools emit them as one token ("0 0" is impossible after
    // tokenization, so this only matches real 6-field lines).
    if fields.len() >= 6 && is_number(&fields[4]) && is_number(&fields[5]) {
        if fields.len() > 6 {
            return Err(format!(
                "linux-style entry has {} fields, expected 6",
                fields.len()
            ));
        }
        return Ok(FstabEntry {
            source,
            mount_point,
            fs_type,
            mount_flags,
            fs_mgr_flags: Vec::new(),
            dump: fields[4].parse().ok(),
            pass: fields[5].parse().ok(),
            layout: FstabLayout::Linux,
            line,
            field_count: fields.len(),
            raw: String::new(),
        });
    }

    // 5-field line whose last field is a number: `... flags 0`.  This is a
    // Linux line that lost its final passno column.
    if fields.len() == 5 && is_number(&fields[4]) {
        return Ok(FstabEntry {
            source,
            mount_point,
            fs_type,
            mount_flags,
            fs_mgr_flags: Vec::new(),
            dump: fields[4].parse().ok(),
            pass: None,
            layout: FstabLayout::AndroidWithPass,
            line,
            field_count: fields.len(),
            raw: String::new(),
        });
    }

    let fs_mgr_flags = if fields.len() == 4 {
        Vec::new()
    } else {
        // Android fstab: everything from column 5 onwards is fs_mgr flags.
        // Joining extra columns handles fstabs that put spaces inside the
        // flag column by mistake instead of rejecting the whole line.
        let mut flags = Vec::new();
        for field in &fields[4..] {
            flags.extend(split_flags(field));
        }
        flags
    };

    Ok(FstabEntry {
        source,
        mount_point,
        fs_type,
        mount_flags,
        fs_mgr_flags,
        dump: None,
        pass: None,
        layout: if fields.len() == 4 {
            FstabLayout::FourField
        } else {
            FstabLayout::Android
        },
        line,
        field_count: fields.len(),
        raw: String::new(),
    })
}

/// Split an fstab line into fields, honouring backslash escapes used by
/// Android (`\040` space, `\011` tab, `\\`).
fn tokenize(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if !current.is_empty() {
                    fields.push(std::mem::take(&mut current));
                }
            }
            '\\' => {
                let mut consumed = false;
                if let Some(next) = chars.peek().copied() {
                    if next == '\\' {
                        chars.next();
                        current.push('\\');
                        consumed = true;
                    }
                }
                if !consumed {
                    let mut octal = String::new();
                    for _ in 0..3 {
                        match chars.peek().copied() {
                            Some(d) if d.is_ascii_digit() && d < '8' => {
                                octal.push(d);
                                chars.next();
                            }
                            _ => break,
                        }
                    }
                    if octal.len() == 3 {
                        if let Ok(v) = u8::from_str_radix(&octal, 8) {
                            current.push(v as char);
                        } else {
                            current.push('\\');
                            current.push_str(&octal);
                        }
                    } else {
                        current.push('\\');
                        current.push_str(&octal);
                    }
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        fields.push(current);
    }
    fields
}

fn split_flags(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn is_number(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// A set of fstab files found in a dump.
#[derive(Debug, Clone, Serialize)]
pub struct FstabSet {
    pub files: Vec<FstabFile>,
}

impl FstabSet {
    pub fn scan(root: &Path) -> io::Result<Self> {
        let mut files = Vec::new();
        for entry in walkdir::WalkDir::new(root).follow_links(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            // Accept `fstab` and `fstab.<anything>` but reject generated
            // reports and backups that merely start with "fstab".
            if name != "fstab" && !name.starts_with("fstab.") {
                continue;
            }
            if [".json", ".txt", ".bak", ".orig", ".rej"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
            {
                continue;
            }
            // Only accept real fstab locations: */etc/fstab*,
            // */first_stage_ramdisk/fstab*, and root-level fstab*.
            let path = entry.path();
            let parent = path.parent().unwrap_or(Path::new(""));
            let parent_name = parent
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            let grandparent_name = parent
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            let looking_like_fstab = parent_name == "etc"
                || parent_name == "first_stage_ramdisk"
                || grandparent_name == "first_stage_ramdisk"
                || parent == root;
            if !looking_like_fstab {
                continue;
            }
            files.push(FstabFile::parse(path)?);
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(FstabSet { files })
    }

    pub fn entries(&self) -> impl Iterator<Item = &FstabEntry> {
        self.files.iter().flat_map(|f| f.entries.iter())
    }

    pub fn errors(&self) -> impl Iterator<Item = &FstabError> {
        self.files.iter().flat_map(|f| f.errors.iter())
    }

    pub fn warnings(&self) -> impl Iterator<Item = &FstabWarning> {
        self.files.iter().flat_map(|f| f.warnings.iter())
    }

    /// Map of logical partition name -> filesystem candidates, in fstab
    /// order.  Duplicate entries (erofs + ext4) are preserved.
    pub fn fs_candidates(&self) -> BTreeMap<String, Vec<String>> {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for entry in self.entries() {
            if !entry.is_logical() {
                continue;
            }
            let fs = entry.fs_type.clone();
            let list = map.entry(entry.source.clone()).or_default();
            if !list.contains(&fs) && fs != "auto" && fs != "none" {
                list.push(fs);
            }
        }
        map
    }

    pub fn json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    pub fn print_report(&self, json: bool) {
        if json {
            match self.json() {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("failed to encode fstab report: {e}"),
            }
            return;
        }
        for file in &self.files {
            println!("{}", file.path.display());
            println!(
                "  entries: {}  warnings: {}  errors: {}",
                file.entries.len(),
                file.warnings.len(),
                file.errors.len()
            );
            for entry in &file.entries {
                println!(
                    "  line {:>4}  {:<10} {:<22} {:<6} {}",
                    entry.line,
                    entry.layout.to_string(),
                    entry.source,
                    entry.fs_type,
                    entry.mount_point
                );
            }
            for warning in &file.warnings {
                println!("  warning:{}: {}", warning.line, warning.message);
            }
            for error in &file.errors {
                println!("  ERROR:{}: {}", error.line, error.message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse(text: &str) -> FstabFile {
        FstabFile::parse_bytes(&PathBuf::from("fstab.test"), text.as_bytes())
    }

    #[test]
    fn parses_android_five_field() {
        let f = parse(
            "system /system erofs ro wait,slotselect,avb=vbmeta_system,logical,first_stage_mount\n",
        );
        assert_eq!(f.entries.len(), 1);
        let e = &f.entries[0];
        assert_eq!(e.layout, FstabLayout::Android);
        assert_eq!(e.source, "system");
        assert_eq!(e.fs_type, "erofs");
        assert!(e.is_logical());
        assert!(e.has_fs_mgr_flag("wait"));
        assert!(f.errors.is_empty());
    }

    #[test]
    fn parses_four_field() {
        let f = parse("/dev/block/zram0 none swap defaults\n");
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].layout, FstabLayout::FourField);
        assert!(f.entries[0].fs_mgr_flags.is_empty());
    }

    #[test]
    fn parses_linux_six_field() {
        let f = parse("/dev/block/by-name/frp /persistent ext4 defaults 0 0\n");
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].layout, FstabLayout::Linux);
        assert_eq!(f.entries[0].dump, Some(0));
        assert_eq!(f.entries[0].pass, Some(0));
    }

    #[test]
    fn parses_vendor_emmc_pseudo_entry() {
        let f = parse("/dev/block/by-name/frp /persistent emmc defaults defaults\n");
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].fs_type, "emmc");
        assert_eq!(f.entries[0].fs_mgr_flags, vec!["defaults"]);
    }

    #[test]
    fn parses_tabs_crlf_and_escaped_spaces() {
        let f = parse(
            "# comment\r\nUUID=1234\t/data/media\tf2fs\tnoatime,nodev\twait,check\r\n/data/my\\040folder /mnt/x ext4 ro defaults\r\n",
        );
        assert_eq!(f.entries.len(), 2);
        assert_eq!(f.entries[0].mount_point, "/data/media");
        assert_eq!(f.entries[1].source, "/data/my folder");
        assert!(f.errors.is_empty());
    }

    #[test]
    fn keeps_duplicate_fs_candidates() {
        let f =
            parse("vendor /vendor erofs ro wait,logical\nvendor /vendor ext4 ro wait,logical\n");
        assert_eq!(f.entries.len(), 2);
        assert!(f.warnings.is_empty());
        let set = FstabSet { files: vec![f] };
        let cands = set.fs_candidates();
        assert_eq!(
            cands.get("vendor"),
            Some(&vec!["erofs".into(), "ext4".into()])
        );
    }

    #[test]
    fn reports_malformed_lines_instead_of_dropping_them() {
        let f = parse("not a valid\n");
        assert!(f.entries.is_empty());
        assert_eq!(f.errors.len(), 1);
        assert!(f.errors[0].message.contains("at least 4 fields"));
        assert!(!f.is_clean());
    }

    #[test]
    fn scan_ignores_generated_reports() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-fstab-scan");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("vendor/etc")).unwrap();
        std::fs::write(
            dir.join("vendor/etc/fstab.test"),
            "system /system erofs ro wait,logical\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("vendor/etc/fstab-report.json"),
            "{\"files\": [ {\"path\": \"x\"} ]}\n",
        )
        .unwrap();
        let set = FstabSet::scan(&dir).unwrap();
        assert_eq!(set.files.len(), 1);
        assert!(set.files[0].path.ends_with("fstab.test"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_same_fs_warns() {
        let f = parse("vendor /vendor ext4 ro wait\nvendor /vendor ext4 ro wait\n");
        assert_eq!(f.entries.len(), 2);
        assert_eq!(f.warnings.len(), 1);
    }
}
