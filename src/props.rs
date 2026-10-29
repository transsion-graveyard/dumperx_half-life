//! Android `build.prop` parsing and device metadata extraction.

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct PropFile {
    pub path: PathBuf,
    pub props: Vec<(String, String)>,
}

#[derive(Debug, Default)]
pub struct PropStore {
    pub files: Vec<PropFile>,
}

impl PropStore {
    pub fn load(root: &Path) -> Result<Self> {
        let mut files = Vec::new();
        for entry in WalkDir::new(root).follow_links(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            let is_prop = (name.starts_with("build") && name.ends_with(".prop"))
                || name == "default.prop"
                || name == "prop.default"
                || name == "build_default.prop";
            if !is_prop {
                continue;
            }
            let path = entry.path().to_path_buf();
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            let props = parse_prop_file(&path)?;
            files.push(PropFile { path: relative, props });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(PropStore { files })
    }

    /// Return the last value of `key` in any file matching one of the
    /// patterns, trying patterns in order (mirrors the old shell `get_prop`).
    pub fn get(&self, key: &str, patterns: &[&str]) -> Option<String> {
        if patterns.is_empty() {
            return self.files.iter().rev().find_map(|f| {
                f.props
                    .iter()
                    .rev()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.clone())
            });
        }
        for pattern in patterns {
            let mut found = None;
            for file in &self.files {
                let rel = file.path.to_string_lossy();
                if wildcard_match(pattern, &rel) {
                    if let Some((_, value)) = file
                        .props
                        .iter()
                        .rev()
                        .find(|(k, _)| k == key)
                    {
                        found = Some(value.clone());
                    }
                }
            }
            if found.is_some() {
                return found;
            }
        }
        None
    }

    pub fn all_props(&self) -> impl Iterator<Item = (&str, &str)> {
        self.files
            .iter()
            .flat_map(|f| f.props.iter().map(|(k, v)| (k.as_str(), v.as_str())))
    }
}

fn parse_prop_file(path: &Path) -> Result<Vec<(String, String)>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    parse_prop_text(&text, &mut out);
    Ok(out)
}

fn parse_prop_text(text: &str, out: &mut Vec<(String, String)>) {
    for line in text.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("import ") {
            // Imports are resolved by PropStore::load scanning the whole
            // dump, so remember nothing here; if an imported prop lives
            // outside the dump it cannot be evaluated anyway.
            let _ = rest;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        out.push((key.to_string(), value.trim().to_string()));
    }
}

/// Simple `*`/`?` wildcard matcher over relative paths.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 0..p.len() {
        match p[i] {
            '*' => {
                for j in 0..=t.len() {
                    if dp[i][j] {
                        dp[i + 1][j] = true;
                        if j < t.len() {
                            dp[i][j + 1] = true;
                        }
                    }
                }
            }
            '?' => {
                for j in 0..t.len() {
                    if dp[i][j] {
                        dp[i + 1][j + 1] = true;
                    }
                }
            }
            c => {
                for j in 0..t.len() {
                    if dp[i][j] && t[j] == c {
                        dp[i + 1][j + 1] = true;
                    }
                }
            }
        }
    }
    dp[p.len()][t.len()]
}

fn expand_braces(pattern: &str) -> Vec<String> {
    if let Some(open) = pattern.find('{') {
        if let Some(close_rel) = pattern[open..].find('}') {
            let close = open + close_rel;
            let prefix = &pattern[..open];
            let suffix = &pattern[close + 1..];
            let options = &pattern[open + 1..close];
            let mut out = Vec::new();
            for option in options.split(',') {
                let combined = format!("{prefix}{option}{suffix}");
                out.extend(expand_braces(&combined));
            }
            return out;
        }
    }
    vec![pattern.to_string()]
}

fn get(store: &PropStore, key: &str, patterns: &[&str]) -> Option<String> {
    let mut expanded: Vec<String> = Vec::new();
    for pattern in patterns {
        expanded.extend(expand_braces(pattern));
    }
    let refs: Vec<&str> = expanded.iter().map(String::as_str).collect();
    store.get(key, &refs)
}

fn get_or(store: &PropStore, key: &str, patterns: &[&str], default: &str) -> String {
    get(store, key, patterns).unwrap_or_else(|| default.to_string())
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceMetadata {
    pub description: String,
    pub manufacturer: String,
    pub platform: String,
    pub codename: String,
    pub brand: String,
    pub flavor: String,
    pub release: String,
    pub kernel_version: String,
    pub id: String,
    pub incremental: String,
    pub tags: String,
    pub abilist: String,
    pub is_ab: String,
    pub treble_support: String,
    pub locale: String,
    pub density: String,
    pub fingerprint: String,
    pub otaver: String,
    pub branch: String,
    pub repo: String,
}

pub fn extract_metadata(store: &PropStore, kernel_version: Option<&str>) -> DeviceMetadata {
    let flavor = get(
        store,
        "ro.build.flavor",
        &[
            "vendor/build.prop",
            "system/build.prop",
            "system/system/build.prop",
        ],
    )
    .or_else(|| get(store, "ro.vendor.build.flavor", &["vendor/build*.prop"]))
    .or_else(|| {
        get(
            store,
            "ro.build.flavor",
            &["vendor/build*.prop", "system/build*.prop", "system/system/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.system.build.flavor",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.build.type",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .unwrap_or_default();

    let release = get(
        store,
        "ro.build.version.release",
        &[
            "my_manifest/build*.prop",
            "vendor/build*.prop",
            "system/build*.prop",
            "system/system/build*.prop",
        ],
    )
    .or_else(|| get(store, "ro.vendor.build.version.release", &["vendor/build*.prop"]))
    .or_else(|| {
        get(
            store,
            "ro.system.build.version.release",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .unwrap_or_default();

    let id = get(store, "ro.build.id", &["my_manifest/build*.prop"])
        .or_else(|| get(store, "ro.build.id", &["system/system/build_default.prop"]))
        .or_else(|| get(store, "ro.build.id", &["vendor/euclid/my_manifest/build.prop"]))
        .or_else(|| {
            get(
                store,
                "ro.build.id",
                &["vendor/build*.prop", "system/build*.prop", "system/system/build*.prop"],
            )
        })
        .or_else(|| get(store, "ro.vendor.build.id", &["vendor/build*.prop"]))
        .or_else(|| {
            get(
                store,
                "ro.system.build.id",
                &["system/build*.prop", "system/system/build*.prop"],
            )
        })
        .unwrap_or_default();

    let incremental = get(
        store,
        "ro.build.version.incremental",
        &["my_manifest/build*.prop"],
    )
    .or_else(|| {
        get(
            store,
            "ro.build.version.incremental",
            &["system/system/build_default.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.build.version.incremental",
            &["vendor/euclid/my_manifest/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.build.version.incremental",
            &["vendor/build*.prop", "system/build*.prop", "system/system/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.vendor.build.version.incremental",
            &["my_manifest/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.vendor.build.version.incremental",
            &["vendor/euclid/my_manifest/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.vendor.build.version.incremental",
            &["vendor/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.system.build.version.incremental",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.build.version.incremental",
            &["my_product/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.system.build.version.incremental",
            &["my_product/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.vendor.build.version.incremental",
            &["my_product/build*.prop"],
        )
    })
    .unwrap_or_default();

    let tags = get(
        store,
        "ro.build.tags",
        &["vendor/build*.prop", "system/build*.prop", "system/system/build*.prop"],
    )
    .or_else(|| get(store, "ro.vendor.build.tags", &["vendor/build*.prop"]))
    .or_else(|| {
        get(
            store,
            "ro.system.build.tags",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .unwrap_or_default();

    let platform = get(
        store,
        "ro.board.platform",
        &["vendor/build*.prop", "system/build*.prop", "system/system/build*.prop"],
    )
    .filter(|v| v != "common")
    .or_else(|| {
        get(
            store,
            "ro.vendor.board.platform",
            &["vendor/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.system.board.platform",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .unwrap_or_default();

    let manufacturer = get(
        store,
        "ro.product.odm.manufacturer",
        &["odm/etc/build*.prop"],
    )
    .or_else(|| {
        get(
            store,
            "ro.product.manufacturer",
            &["odm/etc/fingerprint/build.default.prop"],
        )
    })
    .or_else(|| get(store, "ro.product.manufacturer", &["my_product/build*.prop"]))
    .or_else(|| get(store, "ro.product.manufacturer", &["my_manifest/build*.prop"]))
    .or_else(|| {
        get(
            store,
            "ro.product.manufacturer",
            &["system/system/build_default.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.manufacturer",
            &["vendor/euclid/my_manifest/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.manufacturer",
            &["vendor/build*.prop", "system/build*.prop", "system/system/build*.prop"],
        )
    })
    .or_else(|| get(store, "ro.product.brand.sub", &["my_product/build*.prop"]))
    .or_else(|| {
        get(
            store,
            "ro.product.brand.sub",
            &["system/system/euclid/my_product/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.vendor.product.manufacturer",
            &["vendor/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.vendor.manufacturer",
            &["my_manifest/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.vendor.manufacturer",
            &["system/system/build_default.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.vendor.manufacturer",
            &["vendor/euclid/my_manifest/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.vendor.manufacturer",
            &["vendor/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.system.product.manufacturer",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.system.manufacturer",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.odm.manufacturer",
            &["my_manifest/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.odm.manufacturer",
            &["system/system/build_default.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.odm.manufacturer",
            &["vendor/euclid/my_manifest/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.odm.manufacturer",
            &["vendor/odm/etc/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.manufacturer",
            &["oppo_product/build*.prop", "my_product/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.manufacturer",
            &["vendor/euclid/*/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.system.product.manufacturer",
            &["vendor/euclid/*/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.product.manufacturer",
            &["vendor/euclid/product/build*.prop"],
        )
    })
    .unwrap_or_default();

    let fingerprint = get(
        store,
        "ro.odm.build.fingerprint",
        &["odm/etc/*build*.prop"],
    )
    .or_else(|| {
        get(
            store,
            "ro.vendor.build.fingerprint",
            &["my_manifest/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.vendor.build.fingerprint",
            &["system/system/build_default.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.vendor.build.fingerprint",
            &["vendor/euclid/my_manifest/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.vendor.build.fingerprint",
            &["odm/etc/fingerprint/build.default.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.vendor.build.fingerprint",
            &["vendor/build*.prop"],
        )
    })
    .or_else(|| get(store, "ro.build.fingerprint", &["my_manifest/build*.prop"]))
    .or_else(|| {
        get(
            store,
            "ro.build.fingerprint",
            &["system/system/build_default.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.build.fingerprint",
            &["vendor/euclid/my_manifest/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.build.fingerprint",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.build.fingerprint",
            &["product/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.system.build.fingerprint",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .or_else(|| get(store, "ro.build.fingerprint", &["my_product/build.prop"]))
    .or_else(|| {
        get(
            store,
            "ro.system.build.fingerprint",
            &["my_product/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.vendor.build.fingerprint",
            &["my_product/build.prop"],
        )
    })
    .unwrap_or_default();

    let codename = get(store, "ro.product.odm.device", &["odm/etc/build*.prop"])
        .or_else(|| {
            get(
                store,
                "ro.product.odm.device",
                &["system/system/build_default.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.device",
                &["odm/etc/fingerprint/build.default.prop"],
            )
        })
        .or_else(|| get(store, "ro.product.device", &["my_manifest/build*.prop"]))
        .or_else(|| {
            get(
                store,
                "ro.product.device",
                &["system/system/build_default.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.device",
                &["vendor/euclid/my_manifest/build.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.device",
                &["oppo_product/build*.prop", "my_product/build*.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.product.device",
                &["oppo_product/build*.prop", "my_product/build*.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.vendor.device",
                &["system/system/build_default.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.vendor.device",
                &["vendor/euclid/my_manifest/build.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.vendor.product.device",
                &["system/system/build_default.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.vendor.product.device",
                &["vendor/build*.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.vendor.device",
                &["vendor/build*.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.device",
                &["vendor/build*.prop", "system/build*.prop", "system/system/build*.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.vendor.product.device.oem",
                &["odm/build.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.vendor.product.device.oem",
                &["vendor/euclid/odm/build.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.vendor.device",
                &["my_manifest/build*.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.system.device",
                &["system/build*.prop", "system/system/build*.prop"],
            )
        })
        .or_else(|| {
            get(store, "ro.product.system.device", &["vendor/euclid/*/build.prop"])
        })
        .or_else(|| {
            get(
                store,
                "ro.product.product.device",
                &["vendor/euclid/*/build.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.product.device",
                &["system/system/build_default.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.product.model",
                &["vendor/euclid/*/build.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.system.device",
                &["my_product/build*.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.product.vendor.device",
                &["my_product/build*.prop"],
            )
        })
        .or_else(|| {
            get(
                store,
                "ro.build.fota.version",
                &["system/build*.prop", "system/system/build*.prop"],
            )
            .map(|v| v.split('-').next().unwrap_or(&v).to_string())
        })
        .or_else(|| {
            get(
                store,
                "ro.build.product",
                &["vendor/build*.prop", "system/build*.prop", "system/system/build*.prop"],
            )
        })
        .or_else(|| {
            fingerprint
                .split('/')
                .nth(2)
                .map(|s| s.split(':').next().unwrap_or(s).to_string())
        })
        .unwrap_or_default();

    let mut brand = get(
        store,
        "ro.product.odm.brand",
        &["odm/etc/build*.prop"],
    )
    .or_else(|| {
        get(
            store,
            "ro.product.odm.brand",
            &[&format!("odm/etc/{codename}_build.prop")],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.odm.brand",
            &["system/system/build_default.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.brand",
            &["odm/etc/fingerprint/build.default.prop"],
        )
    })
    .or_else(|| get(store, "ro.product.brand", &["my_product/build*.prop"]))
    .or_else(|| {
        get(
            store,
            "ro.product.brand",
            &["system/system/build_default.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.brand",
            &["vendor/euclid/my_manifest/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.brand",
            &["vendor/build*.prop", "system/build*.prop", "system/system/build*.prop"],
        )
    })
    .or_else(|| get(store, "ro.product.brand.sub", &["my_product/build*.prop"]))
    .or_else(|| {
        get(
            store,
            "ro.product.brand.sub",
            &["system/system/euclid/my_product/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.vendor.brand",
            &["my_manifest/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.vendor.brand",
            &["system/system/build_default.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.vendor.brand",
            &["vendor/euclid/my_manifest/build.prop"],
        )
    })
    .or_else(|| get(store, "ro.product.vendor.brand", &["vendor/build*.prop"]))
    .or_else(|| get(store, "ro.vendor.product.brand", &["vendor/build*.prop"]))
    .or_else(|| {
        get(
            store,
            "ro.product.system.brand",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.system.brand",
            &["vendor/euclid/*/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.product.brand",
            &["vendor/euclid/product/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.odm.brand",
            &["my_manifest/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.odm.brand",
            &["vendor/euclid/my_manifest/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.odm.brand",
            &["vendor/odm/etc/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.brand",
            &["oppo_product/build*.prop", "my_product/build*.prop"],
        )
    })
    .or_else(|| fingerprint.split('/').next().map(str::to_string))
    .or_else(|| {
        if manufacturer.is_empty() {
            None
        } else {
            Some(manufacturer.clone())
        }
    })
    .unwrap_or_default();

    if brand == "OPPO" {
        if let Some(v) = get(
            store,
            "ro.product.system.brand",
            &["vendor/euclid/*/build.prop"],
        ) {
            brand = v;
        }
    }

    let description = get(
        store,
        "ro.build.description",
        &["system/build.prop", "system/system/build.prop"],
    )
    .or_else(|| {
        get(
            store,
            "ro.build.description",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.vendor.build.description",
            &["vendor/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.vendor.build.description",
            &["vendor/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.build.description",
            &["product/build.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.product.build.description",
            &["product/build*.prop"],
        )
    })
    .or_else(|| {
        get(
            store,
            "ro.system.build.description",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .unwrap_or_else(|| {
        format!("{flavor} {release} {id} {incremental} {tags}")
            .trim()
            .to_string()
    });

    let abilist = get(
        store,
        "ro.product.cpu.abilist",
        &["system/build*.prop", "system/system/build*.prop"],
    )
    .or_else(|| {
        get(
            store,
            "ro.vendor.product.cpu.abilist",
            &["vendor/build*.prop"],
        )
    })
    .unwrap_or_default();

    let locale = get_or(
        store,
        "ro.product.locale",
        &["system/build*.prop", "system/system/build*.prop"],
        "undefined",
    );
    let density = get_or(
        store,
        "ro.sf.lcd_density",
        &["vendor/build*.prop", "system/build*.prop", "system/system/build*.prop"],
        "undefined",
    );
    let is_ab = get_or(
        store,
        "ro.build.ab_update",
        &["system/build*.prop", "system/system/build*.prop", "vendor/build*.prop"],
        "false",
    );
    let treble_support = get_or(
        store,
        "ro.treble.enabled",
        &["system/build*.prop", "system/system/build*.prop"],
        "false",
    );

    let otaver = get(
        store,
        "ro.build.version.ota",
        &[
            "vendor/euclid/product/build*.prop",
            "oppo_product/build*.prop",
            "system/build*.prop",
            "system/system/build*.prop",
        ],
    )
    .or_else(|| {
        get(
            store,
            "ro.build.fota.version",
            &["system/build*.prop", "system/system/build*.prop"],
        )
    })
    .unwrap_or_default();

    let branch = if !otaver.is_empty() && fingerprint.is_empty() {
        otaver.replace(' ', "-")
    } else {
        description.replace(' ', "-")
    };

    let sanitize = |s: &str| -> String {
        let lower = s.to_lowercase();
        let filtered: String = lower
            .chars()
            .filter(|c| c.is_ascii_graphic() || *c == ' ')
            .collect();
        filtered.replace('_', "-").chars().take(35).collect()
    };
    let platform = sanitize(&platform);
    let codename = sanitize(&codename);
    let manufacturer = sanitize(&manufacturer);
    let repo = format!("{}_{}_dump", brand.to_lowercase(), codename.to_lowercase());
    let repo = repo.replace(' ', "-");

    DeviceMetadata {
        description,
        manufacturer,
        platform,
        codename,
        brand,
        flavor,
        release,
        kernel_version: kernel_version.unwrap_or_default().to_string(),
        id,
        incremental,
        tags,
        abilist,
        is_ab,
        treble_support,
        locale,
        density,
        fingerprint,
        otaver,
        branch,
        repo,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matching() {
        assert!(wildcard_match("vendor/build*.prop", "vendor/build.prop"));
        assert!(wildcard_match("vendor/build*.prop", "vendor/build_odm.prop"));
        assert!(!wildcard_match("vendor/build*.prop", "system/build.prop"));
        assert!(wildcard_match("a/?/c", "a/b/c"));
    }

    #[test]
    fn parses_props_with_last_value_winning() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/test-props");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("system")).unwrap();
        std::fs::write(
            dir.join("system/build.prop"),
            "ro.build.id=OLD\nro.build.id=NEW\n# comment\nro.product.device=test\n",
        )
        .unwrap();
        let store = PropStore::load(&dir).unwrap();
        assert_eq!(
            store.get("ro.build.id", &["system/build.prop"]),
            Some("NEW".into())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
