//! Native port of android_tools' `proprietary-files.sh` classifier.

use crate::proprietary_rules::{RULES, Rule};
use crate::props::DeviceMetadata;
use crate::sha1sum;
use crate::util;
use anyhow::Result;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
enum MiscScope {
    /// Original android_tools behavior: unmatched system_ext + vendor blobs.
    Shell,
    /// aospdtgen behavior: unmatched files from Treble partitions only
    /// (vendor/odm/vendor_dlkm/odm_dlkm), never system_ext/system.
    TrebleOnly,
}

pub fn generate(dump_root: &Path, _meta: &DeviceMetadata, description: &str) -> Result<PathBuf> {
    util::step("Generating proprietary-files.txt");
    let (out, seen) = build_list(
        dump_root,
        description,
        MiscScope::Shell,
        "# All blobs from",
        true,
    )?;
    let out_path = dump_root.join("proprietary-files.txt");
    fs::write(&out_path, &out)?;
    util::info(format!(
        "wrote {} ({} entries, {} symlink(s))",
        out_path.display(),
        seen.len(),
        count_links(&out)
    ));
    write_sha1(
        dump_root,
        &out_path,
        &dump_root.join("proprietary-files.sha1"),
    )?;
    Ok(out_path)
}

/// Generate the proprietary-files.txt that belongs to the AOSP device tree.
/// This follows aospdtgen's partition scope: unmatched system/system_ext/
/// product files are not dumped into Misc, which keeps prebuilt APKs and
/// framework jars out of the list unless a section explicitly asks for them.
pub fn generate_aosp_tree(
    dump_root: &Path,
    _meta: &DeviceMetadata,
    description: &str,
    tree_root: &Path,
) -> Result<()> {
    util::step("Generating AOSP device-tree proprietary-files.txt");
    let (out, seen) = build_list(
        dump_root,
        description,
        MiscScope::TrebleOnly,
        "# Unpinned blobs from",
        false,
    )?;
    let out_path = tree_root.join("proprietary-files.txt");
    fs::write(&out_path, &out)?;
    util::info(format!(
        "wrote {} ({} entries, {} symlink(s))",
        out_path.display(),
        seen.len(),
        count_links(&out)
    ));
    write_sha1(
        dump_root,
        &out_path,
        &tree_root.join("proprietary-files.sha1"),
    )?;
    Ok(())
}

fn count_links(text: &str) -> usize {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once(";SYMLINK="))
        .map(|(_, links)| links.split(',').count())
        .sum()
}

fn write_sha1(dump_root: &Path, list: &Path, dest: &Path) -> Result<()> {
    let text = fs::read_to_string(list)?;
    let mut sha1_text = String::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            sha1_text.push_str(line);
            sha1_text.push('\n');
            continue;
        }
        let without_optional = line.trim_start_matches('-');
        let blob = without_optional
            .split(";SYMLINK=")
            .next()
            .unwrap_or(without_optional);
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
    fs::write(dest, sha1_text)?;
    Ok(())
}

fn build_list(
    dump_root: &Path,
    description: &str,
    misc_scope: MiscScope,
    header: &str,
    log: bool,
) -> Result<(String, HashSet<String>)> {
    let files = collect_files(dump_root)?;
    let symlinks = collect_symlinks(dump_root)?;

    // Map every symlink to its final target, and remember which links point
    // at each target.  A chain A -> B -> C attaches both A and B to C.
    let mut link_to_target: BTreeMap<String, String> = BTreeMap::new();
    let mut target_links: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for symlink in &symlinks {
        link_to_target.insert(symlink.link.clone(), symlink.target.clone());
        target_links
            .entry(symlink.target.clone())
            .or_default()
            .push(symlink.link.clone());
    }
    for links in target_links.values_mut() {
        links.sort();
        links.dedup();
    }

    let mut file_set: HashSet<String> = files.iter().cloned().collect();
    for target in target_links.keys() {
        file_set.insert(target.clone());
    }

    // A rule can match either a real file or a symlink path.  Both resolve
    // to the blob that must be copied; symlink names are never emitted as
    // standalone blobs.
    let mut match_entries: Vec<(String, String)> = files
        .iter()
        .map(|file| (file.clone(), file.clone()))
        .collect();
    for symlink in &symlinks {
        match_entries.push((symlink.link.clone(), symlink.target.clone()));
    }

    let mut sections: BTreeMap<String, Vec<BlobEntry>> = BTreeMap::new();

    // android_tools starts each section with a curated list from
    // helpers/lists/proprietary/, then appends the grep matches.
    let base_dir = util::utils_dir().join("vendor/android_tools/helpers/lists/proprietary");
    if let Ok(entries) = fs::read_dir(&base_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if let Ok(text) = fs::read_to_string(&path) {
                let list = sections.entry(name).or_default();
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Some(entry) = map_entry(line, &link_to_target, &file_set) {
                        list.push(entry);
                    }
                }
            }
        }
    }

    for rule in RULES {
        for (match_path, blob_path) in &match_entries {
            if rule_matches(rule, &match_path.to_lowercase()) {
                sections
                    .entry(rule.section.to_string())
                    .or_default()
                    .push(BlobEntry::new(blob_path.clone()));
            }
        }
    }

    let mut out = String::new();
    out.push_str(&format!("{header} {description}, unless pinned\n"));
    let mut seen: HashSet<String> = HashSet::new();
    // The original script writes one file per section and emits them in
    // alphabetical order (`find ... | sort`), so a blob that matches several
    // sections belongs to the alphabetically first one.
    for (section, entries) in &sections {
        let mut entries = entries.clone();
        entries.sort_by_key(|entry| entry.path.to_lowercase());
        entries.dedup_by(|a, b| a.path == b.path);
        let mut wrote_header = false;
        for entry in entries {
            let exists = file_set.contains(&entry.path);
            if !exists || !seen.insert(entry.path.clone()) {
                continue;
            }
            if !wrote_header {
                out.push_str(&format!("\n# {section}\n"));
                wrote_header = true;
            }
            out.push_str(&format_blob(&entry, &target_links));
            out.push('\n');
        }
    }

    // Remaining blobs become Misc.  The shell script uses system_ext+vendor;
    // aospdtgen only dumps Treble partitions (vendor/odm) and leaves SSI
    // partitions (system/system_ext/product) to the explicit sections.
    let remove = read_list("remove.txt");
    let ignore = read_list("ignore.txt");
    let mut misc = Vec::new();
    for path in &files {
        if seen.contains(path) {
            continue;
        }
        let in_scope = match misc_scope {
            MiscScope::Shell => path.starts_with("system_ext/") || path.starts_with("vendor/"),
            MiscScope::TrebleOnly => {
                path.starts_with("vendor/")
                    || path.starts_with("odm/")
                    || path.starts_with("vendor_dlkm/")
                    || path.starts_with("odm_dlkm/")
            }
        };
        if !in_scope {
            continue;
        }
        // Do not dump generic SSI apps/frameworks/overlays into Misc.
        // aospdtgen excludes these (system_ext is not a Treble partition and
        // its apps/frameworks are not automatically proprietary blobs); a
        // blob that a section explicitly matches is still kept above.
        if is_generic_ssi_app(path) {
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
                out.push_str(&format_blob(&BlobEntry::new(path), &target_links));
                out.push('\n');
            }
        }
    }

    if log {
        util::info(format!(
            "proprietary list: {} entries, {} symlink(s)",
            seen.len(),
            target_links.values().map(Vec::len).sum::<usize>()
        ));
    }
    Ok((out, seen))
}

#[derive(Debug, Clone)]
struct BlobEntry {
    path: String,
    optional: bool,
}

impl BlobEntry {
    fn new(path: String) -> Self {
        BlobEntry {
            path,
            optional: false,
        }
    }
}

fn map_entry(
    line: &str,
    link_to_target: &BTreeMap<String, String>,
    file_set: &HashSet<String>,
) -> Option<BlobEntry> {
    let optional = line.starts_with('-');
    let core = line.trim_start_matches('-');
    let core = core.split(";SYMLINK=").next().unwrap_or(core).trim();
    if core.is_empty() {
        return None;
    }
    let normalized = normalize_dump_path(core);
    let target = link_to_target
        .get(&normalized)
        .cloned()
        .unwrap_or(normalized);
    if file_set.contains(&target) {
        Some(BlobEntry {
            path: target,
            optional,
        })
    } else {
        None
    }
}

#[derive(Debug, Clone)]
struct DumpSymlink {
    link: String,
    target: String,
}

fn collect_symlinks(root: &Path) -> Result<Vec<DumpSymlink>> {
    let mut symlinks = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_symlink() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if skip_rel(&rel) {
            continue;
        }
        let link = normalize_dump_path(&rel);
        if link.is_empty() {
            continue;
        }
        let Some(target_abs) = resolve_symlink_target(root, entry.path()) else {
            continue;
        };
        let target_rel = target_abs
            .strip_prefix(root)
            .unwrap_or(&target_abs)
            .to_string_lossy();
        let target = normalize_dump_path(&target_rel);
        if target.is_empty() || target == link {
            continue;
        }
        symlinks.push(DumpSymlink { link, target });
    }
    symlinks.sort_by(|a, b| a.link.cmp(&b.link));
    symlinks.dedup_by(|a, b| a.link == b.link && a.target == b.target);
    Ok(symlinks)
}

/// Follow a symlink chain until it reaches a regular file.  Absolute
/// Android paths (`/vendor/...`, `/system/...`) are mapped back into the
/// dump tree; relative links are resolved against the link's directory.
fn resolve_symlink_target(root: &Path, link_path: &Path) -> Option<PathBuf> {
    let mut current = link_path.to_path_buf();
    for _ in 0..16 {
        let meta = fs::symlink_metadata(&current).ok()?;
        if !meta.file_type().is_symlink() {
            return if meta.is_file() { Some(current) } else { None };
        }
        let target = fs::read_link(&current).ok()?;
        current = resolve_link_target(root, &current, &target)?;
        if !current.starts_with(root) {
            return None;
        }
    }
    None
}

fn resolve_link_target(root: &Path, current: &Path, target: &Path) -> Option<PathBuf> {
    if target.is_absolute() {
        let stripped = target.strip_prefix("/").ok()?;
        let mut candidates = vec![root.join(stripped)];
        // A dump may be a full tree (vendor/...) or a single partition root
        // (absolute /vendor/... maps to <dump>/...).  It may also contain a
        // nested system/ directory.
        if let Some((prefix, rest)) =
            stripped
                .to_string_lossy()
                .split_once('/')
                .filter(|(prefix, _)| {
                    matches!(
                        *prefix,
                        "system" | "system_ext" | "vendor" | "odm" | "product"
                    )
                })
        {
            candidates.push(root.join(rest));
            if prefix == "system" {
                candidates.push(root.join("system").join("system").join(rest));
            }
        }
        candidates.push(root.join("system").join(stripped));
        candidates
            .into_iter()
            .find(|candidate| fs::symlink_metadata(candidate).is_ok())
    } else {
        let parent = current.parent()?;
        Some(normalize_path(&parent.join(target)))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn normalize_dump_path(path: &str) -> String {
    let mut path = path.trim().replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_string();
    }
    if let Some(rest) = path.strip_prefix("system/system/") {
        path = rest.to_string();
    } else if let Some(rest) = path.strip_prefix("system/") {
        path = rest.to_string();
    }
    path.trim_start_matches('/').to_string()
}

fn skip_rel(rel: &str) -> bool {
    rel.starts_with("aosp-device-tree/")
        || rel.starts_with("twrp-device-tree/")
        || rel.starts_with("tmp/")
        || rel == "all_files.txt"
        || rel == "all_files.sha1"
        || rel == "README.md"
        || rel == "fstab-report.json"
        || rel == "super-info.json"
        || rel.starts_with("proprietary-files")
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
        if skip_rel(&rel) {
            continue;
        }
        // The original script maps system/system/... to ... and system/...
        // to ... so that paths in the output are partition-relative.
        let normalized = normalize_dump_path(&rel);
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

fn is_generic_ssi_app(path: &str) -> bool {
    [
        "system_ext/app/",
        "system_ext/priv-app/",
        "system_ext/framework/",
        "system_ext/overlay/",
        "system/system/app/",
        "system/system/priv-app/",
        "system/system/framework/",
        "system/system/overlay/",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
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

fn format_blob(entry: &BlobEntry, target_links: &BTreeMap<String, Vec<String>>) -> String {
    let lower = entry.path.to_lowercase();
    let prefixed = entry.optional
        || lower.contains("libplatformconfig")
        || lower.contains("apk")
        || lower.contains("jar")
        || lower.contains("etc/vintf/manifest/");
    let mut out = String::new();
    if prefixed {
        out.push('-');
    }
    out.push_str(&entry.path);
    if let Some(links) = target_links.get(&entry.path) {
        if !links.is_empty() {
            out.push_str(";SYMLINK=");
            out.push_str(&links.join(","));
        }
    }
    out
}

fn rule_matches(rule: &Rule, lower_path: &str) -> bool {
    if rule
        .excludes
        .iter()
        .any(|pattern| pat_match(pattern, lower_path))
    {
        return false;
    }
    if !rule.hardware.is_empty() {
        return rule
            .hardware
            .iter()
            .any(|target| hardware_module_match(target, lower_path));
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
    fn excludes_generic_system_ext_apps_but_keeps_vendor_and_odm_misc() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-proprietary-misc");
        let _ = std::fs::remove_dir_all(&dir);
        for path in [
            "system_ext/app/Foo/Foo.apk",
            "system_ext/framework/foo.jar",
            "system_ext/lib64/libssi.so",
            "vendor/app/Bar/Bar.apk",
            "vendor/lib64/libvendor.so",
            "odm/lib64/libodm.so",
        ] {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, b"x").unwrap();
        }
        let meta = crate::props::extract_metadata(&crate::props::PropStore::default(), None);
        generate(&dir, &meta, "test").unwrap();
        let root = std::fs::read_to_string(dir.join("proprietary-files.txt")).unwrap();
        assert!(!root.contains("system_ext/app/Foo/Foo.apk"));
        assert!(!root.contains("system_ext/framework/foo.jar"));
        assert!(root.contains("system_ext/lib64/libssi.so"));
        assert!(root.contains("vendor/app/Bar/Bar.apk"));
        assert!(root.contains("vendor/lib64/libvendor.so"));
        assert!(!root.contains("odm/lib64/libodm.so"));

        generate_aosp_tree(&dir, &meta, "test", &dir).unwrap();
        let tree = std::fs::read_to_string(dir.join("proprietary-files.txt")).unwrap();
        assert!(!tree.contains("system_ext/app/Foo/Foo.apk"));
        assert!(!tree.contains("system_ext/lib64/libssi.so"));
        assert!(tree.contains("vendor/app/Bar/Bar.apk"));
        assert!(tree.contains("vendor/lib64/libvendor.so"));
        assert!(tree.contains("odm/lib64/libodm.so"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emits_symlinks_once_on_the_target_blob() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".work/test-proprietary-symlinks");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("vendor/lib64/mt6789")).unwrap();
        std::fs::create_dir_all(dir.join("vendor/bin")).unwrap();
        std::fs::write(dir.join("vendor/lib64/mt6789/libfoo.so"), b"foo").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("mt6789/libfoo.so", dir.join("vendor/lib64/libfoo.so"))
                .unwrap();
            std::os::unix::fs::symlink(
                "/vendor/lib64/mt6789/libfoo.so",
                dir.join("vendor/bin/foo"),
            )
            .unwrap();
        }
        let meta = crate::props::extract_metadata(&crate::props::PropStore::default(), None);
        generate(&dir, &meta, "test").unwrap();
        let text = std::fs::read_to_string(dir.join("proprietary-files.txt")).unwrap();
        assert!(text.contains(
            "vendor/lib64/mt6789/libfoo.so;SYMLINK=vendor/bin/foo,vendor/lib64/libfoo.so"
        ));
        assert!(!text.lines().any(|line| line == "vendor/lib64/libfoo.so"));
        assert!(!text.lines().any(|line| line == "vendor/bin/foo"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_absolute_system_symlinks_in_nested_and_root_layouts() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-system-symlinks");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("system/system/bin")).unwrap();
        std::fs::write(dir.join("system/system/bin/tool"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/system/bin/tool", dir.join("system/system/bin/tool_alias"))
            .unwrap();
        let links = collect_symlinks(&dir).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].link, "bin/tool_alias");
        assert_eq!(links[0].target, "bin/tool");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hardware_module_match_works_for_hw_prefix() {
        assert!(hardware_module_match(
            "hw/camera",
            "vendor/lib64/hw/camera.mt6789.so"
        ));
        assert!(!hardware_module_match(
            "hw/camera",
            "vendor/lib64/libfoo.so"
        ));
        assert!(hardware_module_match(
            "hardware.audio",
            "vendor/lib64/android.hardware.audio.common-util.so"
        ));
    }
}
