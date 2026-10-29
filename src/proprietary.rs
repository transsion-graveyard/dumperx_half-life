//! Native port of android_tools' `proprietary-files.sh` classifier.

use crate::props::DeviceMetadata;
use crate::proprietary_rules::{Rule, RULES};
use crate::sha1sum;
use crate::util;
use anyhow::Result;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub fn generate(
    dump_root: &Path,
    _meta: &DeviceMetadata,
    description: &str,
) -> Result<PathBuf> {
    util::step("Generating proprietary-files.txt");
    let files = collect_files(dump_root)?;
    let file_set: HashSet<String> = files.iter().cloned().collect();
    let lower: Vec<String> = files.iter().map(|f| f.to_lowercase()).collect();
    let mut sections: BTreeMap<String, Vec<String>> = BTreeMap::new();

    // android_tools starts each section with a curated list from
    // helpers/lists/proprietary/, then appends the grep matches.
    let base_dir = util::utils_dir().join("vendor/android_tools/helpers/lists/proprietary");
    if let Ok(entries) = fs::read_dir(&base_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if let Ok(text) = fs::read_to_string(&path) {
                let list = sections.entry(name).or_default();
                for line in text.lines() {
                    let line = line.trim();
                    if !line.is_empty() {
                        list.push(line.to_string());
                    }
                }
            }
        }
    }

    for rule in RULES {
        for (path, lower_path) in files.iter().zip(lower.iter()) {
            if rule_matches(rule, lower_path) {
                sections
                    .entry(rule.section.to_string())
                    .or_default()
                    .push(path.clone());
            }
        }
    }

    let mut out = String::new();
    out.push_str(&format!("# All blobs from {description}, unless pinned\n"));
    let mut seen: HashSet<String> = HashSet::new();
    // The original script writes one file per section and emits them in
    // alphabetical order (`find ... | sort`), so a blob that matches several
    // sections belongs to the alphabetically first one.
    for (section, entries) in &sections {
        let mut entries = entries.clone();
        entries.sort_by_key(|s| s.to_lowercase());
        entries.dedup();
        let mut wrote_header = false;
        for entry in entries {
            let exists = entry
                .strip_prefix('-')
                .map(|e| file_set.contains(e))
                .unwrap_or_else(|| file_set.contains(&entry));
            if !exists {
                continue;
            }
            if !seen.insert(entry.clone()) {
                continue;
            }
            if !wrote_header {
                out.push_str(&format!("\n# {section}\n"));
                wrote_header = true;
            }
            out.push_str(&format_blob(&entry));
            out.push('\n');
        }
    }

    // Remaining system_ext and vendor blobs become Misc, exactly like the
    // original script.
    let remove = read_list("remove.txt");
    let ignore = read_list("ignore.txt");
    let mut misc = Vec::new();
    for path in &files {
        if seen.contains(path) {
            continue;
        }
        if !(path.starts_with("system_ext/") || path.starts_with("vendor/")) {
            continue;
        }
        if remove.iter().any(|prefix| path.starts_with(prefix)) {
            continue;
        }
        if ignore.iter().any(|line| line == path) {
            continue;
        }
        misc.push(path.clone());
    }
    if !misc.is_empty() {
        out.push_str("\n# Misc\n");
        misc.sort();
        for path in misc {
            if seen.insert(path.clone()) {
                out.push_str(&format_blob(&path));
                out.push('\n');
            }
        }
    }

    let out_path = dump_root.join("proprietary-files.txt");
    fs::write(&out_path, out)?;
    util::info(format!(
        "wrote {} ({} entries)",
        out_path.display(),
        seen.len()
    ));

    // proprietary-files.sha1
    let sha1_path = dump_root.join("proprietary-files.sha1");
    let mut sha1_text = format!(
        "# All blobs are from \"{description}\" and are pinned with sha1sum values\n"
    );
    for line in fs::read_to_string(&out_path)?.lines() {
        if line.is_empty() || line.starts_with('#') {
            sha1_text.push_str(line);
            sha1_text.push('\n');
            continue;
        }
        let blob = line.trim_start_matches('-');
        if let Some(path) = sha1sum::resolve_blob(dump_root, blob) {
            if let Ok(hash) = sha1sum::sha1_file(&path) {
                sha1_text.push_str(line);
                sha1_text.push('|');
                sha1_text.push_str(&hash);
                sha1_text.push('\n');
                continue;
            }
        }
        sha1_text.push_str(line);
        sha1_text.push('\n');
    }
    fs::write(&sha1_path, sha1_text)?;
    Ok(out_path)
}

fn collect_files(root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with("aosp-device-tree/")
            || rel.starts_with("twrp-device-tree/")
            || rel.starts_with("tmp/")
            || rel == "all_files.txt"
            || rel == "all_files.sha1"
            || rel == "README.md"
            || rel == "fstab-report.json"
            || rel == "super-info.json"
            || rel.starts_with("proprietary-files")
        {
            continue;
        }
        // The original script maps system/system/... to ... and system/...
        // to ... so that paths in the output are partition-relative.
        let normalized = if let Some(rest) = rel.strip_prefix("system/system/") {
            rest.to_string()
        } else if let Some(rest) = rel.strip_prefix("system/") {
            rest.to_string()
        } else {
            rel.clone()
        };
        if normalized.starts_with("vendor/bin/") && normalized.ends_with(".sh") {
            continue;
        }
        if normalized.contains("/oat/") {
            continue;
        }
        if normalized.is_empty() {
            continue;
        }
        files.push(normalized);
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn read_list(name: &str) -> Vec<String> {
    let path = util::utils_dir()
        .join("vendor/android_tools/helpers/lists")
        .join(name);
    fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn format_blob(path: &str) -> String {
    let lower = path.to_lowercase();
    let prefixed = lower.contains("libplatformconfig")
        || lower.contains("apk")
        || lower.contains("jar")
        || lower.contains("etc/vintf/manifest/");
    if prefixed {
        format!("-{path}")
    } else {
        path.to_string()
    }
}

fn rule_matches(rule: &Rule, lower_path: &str) -> bool {
    if rule.excludes.iter().any(|pattern| pat_match(pattern, lower_path)) {
        return false;
    }
    if !rule.hardware.is_empty() {
        return rule.hardware.iter().any(|target| {
            hardware_module_match(target, lower_path)
        });
    }
    rule.include_groups
        .iter()
        .all(|group| pat_match(group, lower_path))
}

fn hardware_module_match(target: &str, path: &str) -> bool {
    if let Some(rest) = target.strip_prefix("hw/") {
        (path.contains("odm/") || path.contains("vendor/"))
            && (path.contains("lib/") || path.contains("lib64/"))
            && path.contains(&format!("hw/{rest}"))
    } else {
        let in_partition = path.contains("odm/")
            || path.contains("vendor/")
            || path.contains("product/")
            || path.contains("system_ext/");
        let in_dir = path.contains("bin/hw/")
            || path.contains("etc/init/")
            || path.contains("etc/vintf/manifest/")
            || path.contains("framework/")
            || path.contains("lib/")
            || path.contains("lib64/");
        in_partition && in_dir && (path.contains(target) || path.contains(&format!("hw/{target}")))
    }
}

/// Case-insensitive alternation matcher used by the grep -iE rules.  The
/// original patterns are simple literal alternations (with occasional
/// `.*\.so$` style anchors), so a literal matcher is sufficient.
fn pat_match(group: &str, path: &str) -> bool {
    group.split('|').any(|alt| {
        let alt = clean_pattern(alt);
        !alt.is_empty() && path.contains(&alt)
    })
}

fn clean_pattern(pattern: &str) -> String {
    let mut s = pattern.to_string();
    for prefix in ["^", ".*"] {
        s = s.replace(prefix, "");
    }
    for suffix in ["$", ".*"] {
        s = s.replace(suffix, "");
    }
    s = s.replace("\\.", ".");
    s = s.replace("\\", "");
    // ERE quirk: android_tools writes "libc++" which grep treats as "libc"
    // followed by quantifiers, effectively matching any path containing
    // "libc" (for example libcustom_nvram.so).
    s = s.replace("libc++", "libc");
    s.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_literal_alternations() {
        assert!(pat_match("libfoo|libbar", "vendor/lib64/libbar.so"));
        assert!(!pat_match("libfoo|libbar", "vendor/lib64/libbaz.so"));
        assert!(pat_match(".*\\.so$", "vendor/lib64/libbaz.so"));
    }

    #[test]
    fn hardware_module_match_works_for_hw_prefix() {
        assert!(hardware_module_match(
            "hw/camera",
            "vendor/lib64/hw/camera.mt6789.so"
        ));
        assert!(!hardware_module_match("hw/camera", "vendor/lib64/libfoo.so"));
        assert!(hardware_module_match(
            "hardware.audio",
            "vendor/lib64/android.hardware.audio.common-util.so"
        ));
    }
}
