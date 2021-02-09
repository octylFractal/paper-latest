#![deny(warnings)]

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io::{stdout, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::str::FromStr;

use anyhow::Context;
use console::{colors_enabled_stderr, set_colors_enabled};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::Digest;
use structopt::StructOpt;

use crate::progress::{new_progress_bar, ProgressTrackable};

mod progress;

#[derive(StructOpt)]
#[structopt(name = "paper-latest", about = "Gets the latest Paper JAR")]
struct PaperLatest {
    #[structopt(short, long, help = "The project to fetch", default_value = "paper")]
    project: String,
    #[structopt(
        long,
        help = "The type of download to fetch",
        default_value = "application"
    )]
    download_type: String,
    #[structopt(help = "The version (group) to fetch")]
    version: String,
    #[structopt(
        help = "The file location to download to, or `-` for STDOUT",
        default_value = "-"
    )]
    download_location: DownloadLocation,
}

#[derive(Clone)]
enum DownloadLocation {
    Stdout,
    File(PathBuf),
}

impl DownloadLocation {
    fn writer(&self) -> Result<Box<dyn Write>, anyhow::Error> {
        Ok(match self {
            DownloadLocation::Stdout => Box::new(stdout()),
            DownloadLocation::File(path) => Box::new(std::fs::File::create(path)?),
        })
    }
}

impl FromStr for DownloadLocation {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "-" => Ok(DownloadLocation::Stdout),
            _ => Ok(DownloadLocation::File(PathBuf::from(s))),
        }
    }
}

impl Display for DownloadLocation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadLocation::Stdout => write!(f, "Standard Out"),
            DownloadLocation::File(path) => path.display().fmt(f),
        }
    }
}

const PAPER_BASE: &str = "https://papermc.io/api/v2";

fn main() {
    // hacky af, but we know we don't print color to STDOUT here
    set_colors_enabled(colors_enabled_stderr());
    let args: PaperLatest = PaperLatest::from_args();

    if matches!(args.download_location, DownloadLocation::Stdout) && console::user_attended() {
        eprintln!("Refusing to write binary output to a terminal. Please redirect to another program or file.");
        exit(1);
    }

    let project_data: ProjectData =
        do_get_json(format!("{}/projects/{}", PAPER_BASE, args.project))
            .expect("Failed to get project data");

    let version = determine_version(&project_data, &args.version)
        .expect("Failed to determine version to download");

    let version_data: VersionData = do_get_json(format!(
        "{}/projects/{}/versions/{}",
        PAPER_BASE, args.project, version
    ))
    .expect("Failed to get version data");

    let build = version_data
        .builds
        .into_iter()
        .max()
        .expect("Version has no builds");

    let build_data: BuildData = do_get_json(format!(
        "{}/projects/{}/versions/{}/builds/{}",
        PAPER_BASE, args.project, version, build
    ))
    .expect("Failed to get build data");

    let download = build_data
        .downloads
        .get(&args.download_type)
        .expect("No download of the given type available");

    let download_hash = hex::decode(&download.sha256)
        .with_context(|| format!("Got a sha256 value that wasn't hex: {}", download.sha256))
        .unwrap();

    if let DownloadLocation::File(path) = args.download_location.clone() {
        if path.exists()
            && check_file_hash(&download_hash, &path).unwrap_or_else(|e| {
                eprintln!("Failed to check file hash, re-downloading: {}", e);
                false
            })
        {
            eprintln!("Latest build already downloaded. Exiting.");
            return;
        }
    }

    let bytes =
        download_build(&args, &version, build, download).expect("Failed to download from stream");

    check_mem_hash(&download_hash, &bytes);

    let mut writer = args
        .download_location
        .writer()
        .expect("Failed to open writer to download location");

    let bar = new_progress_bar(Some(bytes.len() as u64));
    bar.set_message("Saving to output");
    let mut bytes_reader = bytes.as_slice().track_with(bar);

    std::io::copy(&mut bytes_reader, &mut writer)
        .with_context(|| format!("Failed to save bytes to {}", args.download_location))
        .unwrap();

    bytes_reader.bar.finish_with_message("Saved.");

    eprintln!(
        "Downloaded PaperMC Project '{}', version '{}', build '{}' to '{}'",
        project_data.project_id, version, build, args.download_location
    );
}

fn download_build(
    args: &PaperLatest,
    version: &String,
    build: i32,
    download: &Download,
) -> Result<Vec<u8>, anyhow::Error> {
    let res = attohttpc::get(format!(
        "{}/projects/{}/versions/{}/builds/{}/downloads/{}",
        PAPER_BASE, args.project, version, build, download.name
    ))
    .send()?
    .error_for_status()?;
    let bar_length = res.headers().get("Content-length").and_then(|len| {
        len.to_str()
            .ok()
            .and_then(|len_str| len_str.parse::<u64>().ok())
    });

    let bar = new_progress_bar(bar_length);
    bar.set_message("Downloading to memory");

    let mut real_reader = res.track_with(bar);

    let mut bytes: Vec<u8> = vec![];
    std::io::copy(&mut real_reader, &mut bytes)?;
    real_reader.bar.finish_with_message("Finished download.");
    Ok(bytes)
}

fn check_file_hash(
    download_hash: &Vec<u8>,
    download_location: &Path,
) -> Result<bool, anyhow::Error> {
    let bar = new_progress_bar(download_location.metadata().map(|m| m.len()).ok());
    bar.set_message("Checking if file is the latest build");
    let mut file_reader = File::open(download_location)?.track_with(bar);

    let disk_hash = {
        let mut sha = sha2::Sha256::new();
        std::io::copy(&mut file_reader, &mut sha)?;
        sha.finalize().to_vec()
    };
    let is_good = download_hash == &disk_hash;

    file_reader.bar.finish_with_message(if is_good {
        "File is latest"
    } else {
        "Need to download"
    });

    Ok(is_good)
}

fn check_mem_hash(download_hash: &Vec<u8>, bytes: &Vec<u8>) {
    let bar = new_progress_bar(Some(bytes.len() as u64));
    bar.set_message("Validating");
    let mut bytes_reader = bytes.as_slice().track_with(bar);

    let memory_hash = {
        let mut sha = sha2::Sha256::new();
        std::io::copy(&mut bytes_reader, &mut sha).unwrap();
        sha.finalize().to_vec()
    };
    let is_good = download_hash == &memory_hash;

    bytes_reader
        .bar
        .finish_with_message(if is_good { "Valid!" } else { "Invalid! :(" });

    if !is_good {
        panic!(
            "Failed digest check, given {}, got {}",
            hex::encode(&download_hash),
            hex::encode(&memory_hash)
        );
    }
}

fn determine_version(
    project_data: &ProjectData,
    version: &String,
) -> Result<String, anyhow::Error> {
    if project_data.version_groups.contains(&version) {
        let group_data: VersionGroupData = do_get_json(format!(
            "{}/projects/{}/version_group/{}",
            PAPER_BASE, project_data.project_id, version
        ))
        .expect("Failed to get version group data");
        if let Some(g) = group_data.versions.into_iter().last() {
            return Ok(g);
        }
    }
    if project_data.versions.contains(&version) {
        Ok(version.clone())
    } else {
        Err(anyhow::anyhow!(
            "{} is not a known version or (part of a) version group",
            version
        ))
    }
}

fn do_get_json<T: DeserializeOwned, U: AsRef<str>>(url: U) -> Result<T, anyhow::Error> {
    attohttpc::get(url)
        .send()
        .and_then(|x| x.error_for_status())
        .and_then(|x| x.json())
        .context("Failed to download JSON")
}

#[derive(Deserialize)]
struct ProjectData {
    project_id: String,
    version_groups: Vec<String>,
    versions: Vec<String>,
}

#[derive(Deserialize)]
struct VersionGroupData {
    versions: Vec<String>,
}

#[derive(Deserialize)]
struct VersionData {
    builds: Vec<i32>,
}

#[derive(Deserialize)]
struct BuildData {
    downloads: HashMap<String, Download>,
}

#[derive(Deserialize)]
struct Download {
    name: String,
    sha256: String,
}
