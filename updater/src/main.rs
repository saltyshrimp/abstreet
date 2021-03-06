use std::collections::BTreeMap;
use std::error::Error;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::process::Command;

use walkdir::WalkDir;

use abstutil::{CmdArgs, DataPacks, Entry, Manifest, Timer};

const MD5_BUF_READ_SIZE: usize = 4096;

#[tokio::main]
async fn main() {
    let mut args = CmdArgs::new();
    let version = args.optional("--version").unwrap_or("dev".to_string());
    if args.enabled("--upload") {
        assert_eq!(version, "dev");
        upload(version);
        return;
    }
    if args.enabled("--dry") {
        just_compare();
        return;
    }
    args.done();
    download(version).await;
}

async fn download(version: String) {
    let data_packs = DataPacks::load_or_create();
    let local = generate_manifest();
    let truth = Manifest::load().filter(data_packs);

    // Anything local need deleting?
    for path in local.entries.keys() {
        if !truth.entries.contains_key(path) {
            rm(&path);
        }
    }

    // Anything missing or needing updating?
    let mut failed = Vec::new();
    for (path, entry) in truth.entries {
        if local.entries.get(&path).map(|x| &x.checksum) != Some(&entry.checksum) {
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
            match curl(&version, &path).await {
                Ok(bytes) => {
                    println!(
                        "> decompress {}, which is {} bytes compressed",
                        path,
                        bytes.len()
                    );
                    let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
                    let mut out = File::create(&path).unwrap();
                    if let Err(err) = std::io::copy(&mut decoder, &mut out) {
                        println!("{}, but continuing", err);
                        failed.push(format!("{} failed: {}", path, err));
                    }
                }
                Err(err) => {
                    println!("{}, but continuing", err);
                    failed.push(format!("{} failed: {}", path, err));
                }
            };
        }
    }
    if !failed.is_empty() {
        // Fail the build.
        panic!("Failed to download stuff: {:?}", failed);
    }
}

fn just_compare() {
    let data_packs = DataPacks::load_or_create();
    let local = generate_manifest();
    let truth = Manifest::load().filter(data_packs);

    // Anything local need deleting?
    for path in local.entries.keys() {
        if !truth.entries.contains_key(path) {
            println!("- Remove {}", path);
        }
    }

    // Anything missing or needing updating?
    for (path, entry) in truth.entries {
        if local.entries.get(&path).map(|x| &x.checksum) != Some(&entry.checksum) {
            println!("- Update {}", path);
        }
    }
}

fn upload(version: String) {
    let remote_base = format!("/home/dabreegster/s3_abst_data/{}", version);

    let local = generate_manifest();
    let remote: Manifest = abstutil::maybe_read_json(
        format!("{}/MANIFEST.json", remote_base),
        &mut Timer::throwaway(),
    )
    .unwrap_or(Manifest {
        entries: BTreeMap::new(),
    });

    // Anything remote need deleting?
    for path in remote.entries.keys() {
        if !local.entries.contains_key(path) {
            rm(&format!("{}/{}.gz", remote_base, path));
        }
    }

    // Anything missing or needing updating?
    for (path, entry) in &local.entries {
        let remote_path = format!("{}/{}.gz", remote_base, path);
        let changed = remote.entries.get(path).map(|x| &x.checksum) != Some(&entry.checksum);
        if changed {
            std::fs::create_dir_all(std::path::Path::new(&remote_path).parent().unwrap()).unwrap();
            println!("> compressing {}", path);
            {
                let mut input = BufReader::new(File::open(&path).unwrap());
                let out = File::create(&remote_path).unwrap();
                let mut encoder = flate2::write::GzEncoder::new(out, flate2::Compression::best());
                std::io::copy(&mut input, &mut encoder).unwrap();
                encoder.finish().unwrap();
            }
        }
    }

    abstutil::write_json(format!("{}/MANIFEST.json", remote_base), &local);
    abstutil::write_json("data/MANIFEST.json".to_string(), &local);

    must_run_cmd(
        Command::new("aws")
            .arg("s3")
            .arg("sync")
            .arg("--delete")
            .arg(format!("{}/data", remote_base))
            .arg(format!("s3://abstreet/{}/data", version)),
    );
    // Because of the directory structure, do this one separately, without --delete. The wasm files
    // also live in /dev/.
    must_run_cmd(
        Command::new("aws")
            .arg("s3")
            .arg("cp")
            .arg(format!("{}/MANIFEST.json", remote_base))
            .arg(format!("s3://abstreet/{}/MANIFEST.json", version)),
    );
}

fn generate_manifest() -> Manifest {
    let mut kv = BTreeMap::new();
    for entry in WalkDir::new("data/input")
        .into_iter()
        .chain(WalkDir::new("data/system").into_iter())
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_dir() {
            continue;
        }
        let orig_path = entry.path().display().to_string();
        let path = orig_path.replace("\\", "/");
        if path.contains("system/assets/")
            || path.contains("system/fonts")
            || path.contains("system/proposals")
        {
            continue;
        }

        println!("> compute md5sum of {}", path);

        // since these files can be very large, computes the md5 hash in chunks
        let mut file = File::open(&orig_path).unwrap();
        let mut buffer = [0 as u8; MD5_BUF_READ_SIZE];
        let mut context = md5::Context::new();
        let mut size_bytes = 0;
        while let Ok(n) = file.read(&mut buffer) {
            if n == 0 {
                break;
            }
            size_bytes += n;
            context.consume(&buffer[..n]);
        }
        let checksum = format!("{:x}", context.compute());
        kv.insert(
            path,
            Entry {
                checksum,
                size_bytes,
            },
        );
    }
    Manifest { entries: kv }
}

fn must_run_cmd(cmd: &mut Command) {
    println!("> Running {:?}", cmd);
    match cmd.status() {
        Ok(status) => {
            if !status.success() {
                panic!("{:?} failed", cmd);
            }
        }
        Err(err) => {
            panic!("Failed to run {:?}: {:?}", cmd, err);
        }
    }
}

fn rm(path: &str) {
    println!("> rm {}", path);
    match std::fs::remove_file(path) {
        Ok(_) => {}
        Err(e) => match e.kind() {
            std::io::ErrorKind::NotFound => {
                println!("file {} does not exist, continuing", &path);
            }
            other_error => {
                panic!("problem removing file: {:?}", other_error);
            }
        },
    }
}

async fn curl(version: &str, path: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let src = format!(
        "http://abstreet.s3-website.us-east-2.amazonaws.com/{}/{}.gz",
        version, path
    );

    let mut bytes = Vec::new();

    println!("> download {}", src);

    let mut resp = reqwest::get(&src).await.unwrap();

    match resp.error_for_status_ref() {
        Ok(_) => {}
        Err(err) => {
            let err = format!("error getting {}: {}", src, err);
            return Err(err.into());
        }
    };
    while let Some(chunk) = resp.chunk().await.unwrap() {
        bytes.write_all(&chunk).unwrap();
    }

    Ok(bytes)
}
