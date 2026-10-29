#![allow(dead_code)] // incremental Rust port: modules are wired up as the pipeline grows

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod archive;
mod bootimg;
mod cpio;
mod device_tree;
mod fsimage;
mod fstab;
mod ikconfig;
mod legacy;
mod payload;
mod pipeline;
mod props;
mod proprietary;
mod proprietary_rules;
mod ramdisk;
mod sdat;
mod sha1sum;
mod sparse;
mod superimg;
mod util;

#[derive(Parser, Debug)]
#[command(
    name = "dumprx",
    version,
    about = "Offline Android firmware dumper",
    long_about = "DumprX (Rust) extracts Android firmware without downloading anything at runtime.\n\
                  All helper binaries are resolved from ./utils/bin or PATH."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Dump a firmware file, archive or already-extracted directory
    Dump {
        /// Firmware file, archive or extracted dump directory
        input: PathBuf,
        /// Output directory
        #[arg(short, long, default_value = "out")]
        out: PathBuf,
        /// Keep partition images after extracting their filesystems
        #[arg(long)]
        keep_images: bool,
        /// Do not generate device trees
        #[arg(long)]
        no_device_tree: bool,
        /// Generate a TWRP device tree
        #[arg(long)]
        twrp: bool,
        /// Generate an AOSP/LineageOS device tree
        #[arg(long)]
        aosp: bool,
        /// Only extract these super partitions (repeatable)
        #[arg(long = "partition", short = 'p')]
        partitions: Vec<String>,
        /// Verbose command logging
        #[arg(short, long)]
        verbose: bool,
    },
    /// Generate proprietary-files.txt/.sha1 for an existing dump
    Proprietary {
        /// Dump root
        dump: PathBuf,
    },
    /// Generate TWRP/AOSP device trees from an existing dump
    DeviceTree {
        /// Dump root (the directory containing system/, vendor/, boot.img, ...)
        dump: PathBuf,
        /// Only generate the TWRP tree
        #[arg(long)]
        twrp: bool,
        /// Only generate the AOSP tree
        #[arg(long)]
        aosp: bool,
    },
    /// Print boot/vendor_boot image info (supports header v0 through v4)
    Info {
        /// Path to boot.img / vendor_boot.img
        image: PathBuf,
    },
    /// Unpack a boot/vendor_boot image (v0-v4, incl. vendor ramdisk fragments)
    UnpackBoot {
        image: PathBuf,
        /// Output directory
        #[arg(short, long, default_value = "out")]
        out: PathBuf,
    },
    /// Parse and validate fstab files (all Android/Linux layouts)
    Fstab {
        /// Files or directories to scan
        paths: Vec<PathBuf>,
        /// Emit JSON instead of a human readable report
        #[arg(long)]
        json: bool,
    },
    /// Split a dtbo image into individual DTBs
    SplitDtbo {
        image: PathBuf,
        #[arg(short, long, default_value = "dtbo")]
        out: PathBuf,
    },
    /// List logical partitions in a super image (liblp metadata)
    SuperList {
        image: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Extract logical partitions from a super image
    SuperExtract {
        image: PathBuf,
        #[arg(short, long, default_value = "out")]
        out: PathBuf,
        #[arg(long, default_value = "auto")]
        slot: String,
        /// Only extract these partitions (repeatable, accepts names with or
        /// without the _a/_b suffix)
        #[arg(long = "partition", short = 'p')]
        partitions: Vec<String>,
        /// Input is a full disk image; add the block device's
        /// first_logical_sector to extent offsets
        #[arg(long)]
        disk_image: bool,
    },
    /// Convert an Android sparse image to a raw image
    Unsparse {
        image: PathBuf,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Dump {
            input,
            out,
            keep_images,
            no_device_tree,
            twrp,
            aosp,
            partitions,
            verbose,
        } => {
            let opts = pipeline::DumpOptions {
                input,
                out,
                keep_images,
                verbose,
                device_tree: !no_device_tree,
                twrp,
                aosp,
                jobs: std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4),
                partitions,
            };
            pipeline::Pipeline::new(opts)?.run()?;
        }
        Commands::Proprietary { dump } => {
            let store = props::PropStore::load(&dump)?;
            let meta = props::extract_metadata(&store, None);
            proprietary::generate(&dump, &meta, &meta.description)?;
        }
        Commands::DeviceTree { dump, twrp, aosp } => {
            let do_twrp = twrp || !aosp;
            let do_aosp = aosp || !twrp;
            device_tree::generate(
                &dump,
                &device_tree::DeviceTreeOptions { twrp: do_twrp, aosp: do_aosp },
            )?;
        }
        Commands::Info { image } => {
            let img = bootimg::BootImage::parse(&image)?;
            print!("{}", img.info());
        }
        Commands::UnpackBoot { image, out } => {
            let img = bootimg::BootImage::parse(&image)?;
            let files = img.unpack(&out)?;
            for f in files {
                println!("{} ({} bytes)", out.join(f.name).display(), f.size);
            }
            print!("{}", img.info());
        }
        Commands::Fstab { paths, json } => {
            let mut files = Vec::new();
            for path in &paths {
                if path.is_dir() {
                    let set = fstab::FstabSet::scan(path)?;
                    files.extend(set.files);
                } else {
                    files.push(fstab::FstabFile::parse(path)?);
                }
            }
            let set = fstab::FstabSet { files };
            set.print_report(json);
            if set.errors().next().is_some() {
                std::process::exit(2);
            }
        }
        Commands::SplitDtbo { image, out } => {
            let files = bootimg::split_dtbo(&image, &out)?;
            for f in files {
                println!("{} ({} bytes at {:#x})", out.join(f.name).display(), f.size, f.offset);
            }
        }
        Commands::SuperList { image, json } => {
            let mut file = std::fs::File::open(&image)?;
            let meta = superimg::read_metadata(&mut file)?;
            let infos = superimg::partition_infos(&meta);
            if json {
                println!("{}", serde_json::to_string_pretty(&infos)?);
            } else {
                println!(
                    "metadata version: {}.{}  partitions: {}  groups: {}  block devices: {}",
                    meta.major,
                    meta.minor,
                    meta.partitions.len(),
                    meta.groups.len(),
                    meta.block_devices.len()
                );
                println!("Partition table:");
                for p in &infos {
                    println!(
                        "  {:<28} {:>12} bytes  [{}]",
                        p.name,
                        p.size,
                        p.attributes
                    );
                    for e in &p.extents {
                        println!(
                            "      {} sectors -> target {} data {} source {}",
                            e.num_sectors, e.target_type, e.target_data, e.target_source
                        );
                    }
                }
                println!("Block devices:");
                for (i, b) in meta.block_devices.iter().enumerate() {
                    println!(
                        "  [{i}] name='{}' first_logical_sector={} size={} flags={:#x}",
                        b.partition_name, b.first_logical_sector, b.size, b.flags
                    );
                }
                println!("Groups:");
                for g in &meta.groups {
                    println!("  {} max_size={} flags={:#x}", g.name, g.maximum_size, g.flags);
                }
            }
        }
        Commands::SuperExtract { image, out, slot, partitions, disk_image } => {
            let slot = superimg::SlotMode::parse(&slot)?;
            let results =
                superimg::extract_filtered(&image, &out, slot, &partitions, disk_image)?;
            for (name, path, size) in results {
                println!("{name} -> {} ({size} bytes)", path.display());
            }
        }
        Commands::Unsparse { image, out } => {
            let out = out.unwrap_or_else(|| image.with_extension("raw"));
            if sparse::unsparse(&image, &out)? {
                println!("{}", out.display());
            } else {
                eprintln!("{} is not an Android sparse image", image.display());
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
