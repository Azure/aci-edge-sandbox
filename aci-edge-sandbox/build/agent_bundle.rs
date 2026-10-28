// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Stages the edge agent bundle when the `agent-bundled` feature is enabled.
//!
//! The bundle is a ZIP release asset of this repository: the agent library, OpenVMM, the edge
//! guest kernel and initramfs, the image tool, `SOURCE-MANIFEST.json`, `bundle-manifest.json`,
//! and `SHA256SUMS`. By default the build script downloads the asset that `agent-bundle.json`
//! pins, by release tag and file name, and refuses it unless its SHA-256 is the pinned one.
//!
//! The bundle is verified and staged in `OUT_DIR/aci-edge-sandboxes`, which dependent build
//! scripts read as `DEP_ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR`, and copied to an
//! `aci-edge-sandboxes` directory in the Cargo profile directory, beside the executables that
//! Cargo builds. The crate compiles in the SHA-256 digests of the bundle's runtime manifest and
//! agent library, which this script writes to `OUT_DIR` as the `Pins` of `bundle_contract`, and
//! finds the directory relative to the executable at run time, so no absolute path is compiled
//! in.
//!
//! Environment variables, which `bundle_contract` names:
//!
//! - `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL`: a bundle ZIP to download instead of the pinned one; a
//!   `file://` URL names a ZIP on the build host.
//! - `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR`: an extracted bundle to stage instead of downloading
//!   one, for offline builds or locally built bundles.
//! - `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_SHA256`: the approved SHA-256 of the ZIP that
//!   `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL` names; without that variable, it must equal the pin.
//! - `ACI_EDGE_SANDBOXES_AGENT_BUNDLE=skip`: stages nothing, as when the feature is disabled.
//!
//! Targets other than Linux and Windows, where the agent library cannot run, stage nothing, and
//! so do Windows targets other than x86_64 without one of the variables, because the pinned
//! release holds an x86_64 library. Every staged file must match the bundle's `SHA256SUMS`, the
//! bundle may hold no links, and the manifests must agree with the files and name an agent
//! library for the target operating system and architecture. A bundle whose library speaks
//! another sandbox ABI fails the crate's build.

use std::collections::BTreeMap;
use std::env::{self, VarError};
use std::fs::{self, File};
use std::io::{BufReader, ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::bundle_contract::{
    self as contract, ARTIFACTS_DIR_ENV, BESIDE_EXE_DIR as DIRECTORY, BUNDLE_DIR_ENV as DIR,
    BUNDLE_ENV as SKIP, BUNDLE_SHA256_ENV as ARCHIVE_SHA256, BUNDLE_URL_ENV as URL,
    SOURCE_MANIFEST,
};

/// Where this repository's releases are downloaded from.
const RELEASES: &str = "https://github.com/Azure/aci-edge-sandbox/releases/download";
/// The file in this crate's directory that pins the release asset that holds the bundle.
const PIN: &str = "agent-bundle.json";
/// The name of the downloaded archive in `OUT_DIR`.
const ARCHIVE: &str = "aci-edge-sandboxes.zip";
/// The file in `OUT_DIR` that records which bundle this feature configuration installed.
const RECORD: &str = "installed-agent-bundle";
const CHECKSUMS: &str = "SHA256SUMS";
const BUNDLE_MANIFEST: &str = "bundle-manifest.json";

/// The release asset that `agent-bundle.json` pins.
struct Pin {
    /// The tag of the release.
    release: String,
    /// The file name of the asset.
    asset: String,
    /// SHA-256 of the asset, in lowercase hexadecimal digits.
    sha256: String,
}

/// What the crate learns about a verified bundle.
struct Bundle {
    /// Every file that `SHA256SUMS` lists, by its relative path, with its SHA-256.
    files: BTreeMap<String, String>,
    /// SHA-256 of `SOURCE-MANIFEST.json`.
    manifest_sha256: String,
    /// Relative path of the agent library.
    library: String,
    /// SHA-256 of the agent library.
    library_sha256: String,
    /// Version of the sandbox ABI that the library speaks.
    sandbox_abi: u64,
}

/// Stages the bundle that the environment selects, if this build stages one, and writes the pins
/// that the crate compiles in.
pub(crate) fn run() {
    let out_dir = cargo_path("OUT_DIR");
    let bundle = stage(&out_dir);
    write_if_changed(
        &out_dir.join(contract::pins_file!()),
        &pins_source(bundle.as_ref()),
    );
}

/// Stages the bundle that the environment selects and returns it, or `None` if this build stages
/// none.
fn stage(out_dir: &Path) -> Option<Bundle> {
    // `install` dates the files that it writes before this time.
    let started = SystemTime::now();
    for name in [SKIP, URL, DIR, ARCHIVE_SHA256] {
        println!("cargo::rerun-if-env-changed={name}");
    }
    if env::var_os(SKIP).is_some_and(|value| value == "skip") {
        return None;
    }
    let target_os = cargo_variable("CARGO_CFG_TARGET_OS");
    if target_os != "windows" && target_os != "linux" {
        // The agent library runs on Linux and Windows hosts only.
        return None;
    }
    let target_arch = cargo_variable("CARGO_CFG_TARGET_ARCH");
    let downloads = out_dir.join("agent-bundle-download");
    let staged = out_dir.join(DIRECTORY);
    match (variable(DIR), variable(URL)) {
        (Some(_), Some(_)) => fail(format!("set {DIR} or {URL}, not both")),
        (Some(path), None) => {
            let path = PathBuf::from(path);
            // The directory itself may be a link: it is the input that the builder chose.
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => fail(format!(
                    "{DIR} must name an extracted bundle; set {URL} to the file:// URL of a \
                     local ZIP instead"
                )),
                Err(error) => fail(format!("cannot read {DIR}, {}: {error}", path.display())),
            }
            if variable(ARCHIVE_SHA256).is_some() {
                fail(format!(
                    "{ARCHIVE_SHA256} applies to the ZIP that {URL} names, not to {DIR}"
                ));
            }
            println!("cargo::rerun-if-changed={}", path.join(CHECKSUMS).display());
            copy_listed(&path, &staged);
        }
        (None, Some(url)) => match local_path(&url) {
            Some(archive) => {
                match fs::metadata(&archive) {
                    Ok(metadata) if metadata.is_file() => {}
                    Ok(_) => fail(format!(
                        "{URL} names {}, which is not a file",
                        archive.display()
                    )),
                    Err(error) => fail(format!(
                        "cannot read {}, which {URL} names: {error}",
                        archive.display()
                    )),
                }
                println!("cargo::rerun-if-changed={}", archive.display());
                unpack(&archive, &staged, variable(ARCHIVE_SHA256).as_deref());
            }
            None => {
                let approved = variable(ARCHIVE_SHA256);
                let archive = download(&url, &downloads, approved.as_deref());
                unpack(&archive, &staged, approved.as_deref());
            }
        },
        (None, None) => {
            if target_os != "windows" {
                fail(format!(
                    "the pinned release publishes a bundle for Windows only; set {URL} or {DIR} \
                     to a bundle for {target_os}"
                ));
            }
            if target_arch != "x86_64" {
                // An x86_64 library cannot load into this process, so none is staged as usable;
                // `BundledAgent::discover` then reports `backend_unavailable`.
                warn(format!(
                    "the pinned release holds an x86_64 agent library, which cannot run on \
                     {target_arch}; staging no bundle. Set {URL} or {DIR} to a bundle for \
                     {target_arch}"
                ));
                return None;
            }
            let pin = pin();
            if let Some(approved) = variable(ARCHIVE_SHA256)
                && !approved.eq_ignore_ascii_case(&pin.sha256)
            {
                fail(format!(
                    "{ARCHIVE_SHA256} is {approved}, but {PIN} pins {}; set {URL} to use \
                     another bundle",
                    pin.sha256
                ));
            }
            let url = format!("{RELEASES}/{}/{}", pin.release, pin.asset);
            let archive = download(&url, &downloads, Some(&pin.sha256));
            unpack(&archive, &staged, Some(&pin.sha256));
        }
    }
    let bundle = verify(&staged, &target_os, &target_arch);
    install(&staged, out_dir, &bundle, started);
    println!("cargo::metadata=agent_artifacts_dir={}", staged.display());
    Some(bundle)
}

/// Returns the Rust source that defines `PINS`, the `Option<Pins>` of `bundle_contract`, for the
/// pins of `bundle`, or `None` without one.
fn pins_source(bundle: Option<&Bundle>) -> String {
    let pins = bundle.map_or_else(
        || "None".to_owned(),
        |bundle| {
            format!(
                "Some(Pins {{\n    manifest_sha256: {},\n    library: {:?},\n    \
                 library_sha256: {},\n    sandbox_abi: {},\n}})",
                digest_bytes(&bundle.manifest_sha256),
                bundle.library,
                digest_bytes(&bundle.library_sha256),
                bundle.sandbox_abi,
            )
        },
    );
    format!(
        "// Written by the build script of {}.\nconst PINS: Option<Pins> = {pins};\n",
        env!("CARGO_PKG_NAME")
    )
}

/// Returns a SHA-256 digest in lowercase hexadecimal digits as a Rust array of its bytes.
fn digest_bytes(digest: &str) -> String {
    let bytes = aci_edge_sandboxes_model::hex::decode_sha256(digest)
        .unwrap_or_else(|| fail(format!("{digest} is not a SHA-256 digest")));
    format!("{bytes:?}")
}

/// Reads the release asset that `agent-bundle.json` pins, and runs this script again when the
/// pin changes.
fn pin() -> Pin {
    let path = cargo_path("CARGO_MANIFEST_DIR").join(PIN);
    println!("cargo::rerun-if-changed={}", path.display());
    let value = read_json(&path);
    let text = |field: &str| {
        value
            .get(field)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| fail(format!("{PIN} lacks {field}")))
    };
    let pin = Pin {
        release: text("release"),
        asset: text("asset"),
        sha256: text("sha256").to_ascii_lowercase(),
    };
    // Each name becomes one segment of the download URL.
    for (field, name) in [("release", &pin.release), ("asset", &pin.asset)] {
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._+".contains(&byte))
        {
            fail(format!(
                "{PIN} names the {field} {name:?}, which is not a plain name"
            ));
        }
    }
    if aci_edge_sandboxes_model::hex::decode_sha256(&pin.sha256).is_none() {
        fail(format!(
            "{PIN} pins {}, which is not a SHA-256 digest",
            pin.sha256
        ));
    }
    pin
}
/// Downloads `url` into `downloads` and returns the archive, reusing the archive of an
/// earlier run that downloaded the same URL and, if a digest is `approved`, has it.
fn download(url: &str, downloads: &Path, approved: Option<&str>) -> PathBuf {
    create_dir(downloads);
    let archive = downloads.join(ARCHIVE);
    let record = downloads.join(format!("{ARCHIVE}.url"));
    let cached = match fs::read_to_string(&record) {
        Ok(cached) => cached == url,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => {
            warn(format!(
                "cannot read {}: {error}; downloading the bundle again",
                record.display()
            ));
            false
        }
    };
    if cached
        && archive.is_file()
        && approved.is_none_or(|digest| digest.eq_ignore_ascii_case(&sha256(&archive)))
    {
        return archive;
    }
    let partial = downloads.join(format!("{ARCHIVE}.part"));
    execute(
        Command::new("curl")
            .args(["--fail", "--silent", "--show-error", "--location"])
            .args(["--retry", "5", "--retry-delay", "5", "--retry-all-errors"])
            .arg("--output")
            .arg(&partial)
            .arg(url),
    );
    // A record that outlived its archive would vouch for the next one.
    remove_file_if_present(&record);
    fs::rename(&partial, &archive)
        .unwrap_or_else(|error| fail(format!("cannot store {}: {error}", archive.display())));
    fs::write(&record, url)
        .unwrap_or_else(|error| fail(format!("cannot write {}: {error}", record.display())));
    // Cargo shows this again on builds that do not run the build script.
    warn(format!("downloaded {url}"));
    archive
}

/// Checks `archive` against the `approved` digest, if there is one, and extracts it into a new
/// `staged` directory.
fn unpack(archive: &Path, staged: &Path, approved: Option<&str>) {
    if let Some(approved) = approved {
        let actual = sha256(archive);
        if !actual.eq_ignore_ascii_case(approved) {
            fail(format!(
                "SHA-256 mismatch for {}: expected {approved}, found {actual}",
                archive.display()
            ));
        }
    }
    recreate(staged);
    execute(&mut extraction(archive, staged));
}

/// Returns a command that extracts the ZIP `archive` into `directory`.
fn extraction(archive: &Path, directory: &Path) -> Command {
    // GNU tar, the usual `tar` on Linux, cannot read ZIP archives, but bsdtar can.
    if let Some(tar) = [system_tar(), PathBuf::from("bsdtar")]
        .into_iter()
        .find(|candidate| is_bsdtar(candidate))
    {
        let mut command = Command::new(tar);
        command.arg("-xf").arg(archive).arg("-C").arg(directory);
        return command;
    }
    if succeeds(Command::new("unzip").arg("-v")) {
        let mut command = Command::new("unzip");
        command.arg("-q").arg(archive).arg("-d").arg(directory);
        return command;
    }
    fail(format!(
        "cannot extract {}: reading ZIP archives requires bsdtar or unzip on the build host; \
         install one, or set {DIR} to an extracted bundle",
        archive.display()
    ))
}

/// Returns the host's `tar`.
fn system_tar() -> PathBuf {
    // Windows ships bsdtar, which reads ZIP archives; GNU tar from Git for Windows does not.
    if let Some(system_root) = env::var_os("SystemRoot") {
        let system_tar = Path::new(&system_root).join("System32").join("tar.exe");
        if system_tar.is_file() {
            return system_tar;
        }
    }
    PathBuf::from("tar")
}

/// Returns whether `tar` is libarchive's bsdtar, which also reads ZIP archives.
fn is_bsdtar(tar: &Path) -> bool {
    probe(Command::new(tar).arg("--version"))
        .is_some_and(|output| output.status.success() && output.stdout.starts_with(b"bsdtar"))
}

/// Copies the files that the `SHA256SUMS` of `source` lists, and the list itself, into a new
/// `staged` directory. Fails if `source` holds a link, which copying would follow.
fn copy_listed(source: &Path, staged: &Path) {
    reject_links(source);
    let files = read_checksums(&source.join(CHECKSUMS));
    recreate(staged);
    for name in files.keys().map(String::as_str).chain([CHECKSUMS]) {
        copy(&inside(source, name), &inside(staged, name));
    }
}

/// Checks the bundle in `directory` and removes the files that its `SHA256SUMS` does not
/// list.
fn verify(directory: &Path, target_os: &str, target_arch: &str) -> Bundle {
    reject_links(directory);
    let files = read_checksums(&directory.join(CHECKSUMS));
    for (name, expected) in &files {
        let path = inside(directory, name);
        let actual = sha256(&path);
        if &actual != expected {
            fail(format!(
                "SHA-256 mismatch for {}: {CHECKSUMS} lists {expected}, found {actual}",
                path.display()
            ));
        }
    }
    remove_unlisted(directory, directory, &files);
    let listed = |name: &str| {
        files
            .get(name)
            .cloned()
            .unwrap_or_else(|| fail(format!("{CHECKSUMS} does not list {name}")))
    };

    let source = read_json(&directory.join(SOURCE_MANIFEST));
    if source.get("format").and_then(Value::as_u64) != Some(2)
        || source.get("profile").and_then(Value::as_str) != Some("edge")
    {
        fail(format!(
            "{SOURCE_MANIFEST} is not a format-2 manifest of the edge profile"
        ));
    }

    let manifest = read_json(&directory.join(BUNDLE_MANIFEST));
    if manifest
        .pointer("/status/complete")
        .and_then(Value::as_bool)
        != Some(true)
    {
        fail(format!(
            "{BUNDLE_MANIFEST} does not describe a complete bundle"
        ));
    }
    let consumer = |field: &str| {
        manifest
            .pointer(&format!("/consumer/{field}"))
            .unwrap_or_else(|| fail(format!("{BUNDLE_MANIFEST} lacks consumer.{field}")))
    };
    let text = |field: &str| {
        consumer(field)
            .as_str()
            .unwrap_or_else(|| fail(format!("{BUNDLE_MANIFEST} consumer.{field} is not text")))
            .to_ascii_lowercase()
    };
    let manifest_sha256 = listed(SOURCE_MANIFEST);
    if text("runtimeBundleManifest") != SOURCE_MANIFEST.to_ascii_lowercase()
        || text("runtimeBundleManifestSha256") != manifest_sha256
    {
        fail(format!(
            "{BUNDLE_MANIFEST} names another runtime manifest than this {SOURCE_MANIFEST}"
        ));
    }
    let library = consumer("backendLibrary")
        .as_str()
        .unwrap_or_else(|| {
            fail(format!(
                "{BUNDLE_MANIFEST} consumer.backendLibrary is not text"
            ))
        })
        .to_owned();
    let library_sha256 = listed(&library);
    if text("backendLibrarySha256") != library_sha256 {
        fail(format!(
            "{BUNDLE_MANIFEST} and {CHECKSUMS} disagree about {library}"
        ));
    }
    let suffix = if target_os == "windows" {
        ".dll"
    } else {
        ".so"
    };
    if !library.ends_with(suffix) {
        fail(format!(
            "the bundle's agent library, {library}, does not run on {target_os}"
        ));
    }
    check_machine(&inside(directory, &library), target_os, target_arch);
    // The files of the format-2 edge layout, each of which the host checks against its pin.
    let executable = if target_os == "windows" { ".exe" } else { "" };
    for (pin, name) in [
        (
            "/openvmm/executable_sha256",
            format!("bin/openvmm{executable}"),
        ),
        ("/linux/kernel_sha256", "guest/vmlinux".to_owned()),
        (
            "/edge/initramfs_sha256",
            "guest/initramfs-edge.cpio.gz".to_owned(),
        ),
        (
            "/direct_images/executable_sha256",
            format!("bin/direct-images{executable}"),
        ),
        ("/agent/library_sha256", library.clone()),
    ] {
        let pinned = source
            .pointer(pin)
            .and_then(Value::as_str)
            .unwrap_or_else(|| fail(format!("{SOURCE_MANIFEST} lacks {pin}")));
        if !pinned.eq_ignore_ascii_case(&listed(&name)) {
            fail(format!(
                "{SOURCE_MANIFEST} pins {pin} to another digest than {name} has"
            ));
        }
    }
    let sandbox_abi = consumer("sandboxAbiVersion").as_u64().unwrap_or_else(|| {
        fail(format!(
            "{BUNDLE_MANIFEST} consumer.sandboxAbiVersion is not a number"
        ))
    });
    // The crate's compile-time check would also reject the bundle, but only after the bundle
    // had replaced the installed one that earlier executables use.
    let expected_abi = aci_edge_sandboxes_model::wire::SANDBOX_ABI_VERSION;
    if sandbox_abi != u64::from(expected_abi) {
        fail(format!(
            "the bundle's agent library speaks sandbox ABI {sandbox_abi}, but this crate \
             speaks {expected_abi}"
        ));
    }
    Bundle {
        files,
        manifest_sha256,
        library,
        library_sha256,
        sandbox_abi,
    }
}

/// Fails unless the agent library at `path` is built for `target_arch`: a process loads only a
/// library of its own architecture. Reads the machine field of the PE or ELF header.
fn check_machine(path: &Path, target_os: &str, target_arch: &str) {
    // PE `IMAGE_FILE_MACHINE_*` and ELF `EM_*` values.
    let expected: u16 = match (target_os, target_arch) {
        ("windows", "x86_64") => 0x8664,
        ("windows", "aarch64") => 0xaa64,
        ("linux", "x86_64") => 62,
        ("linux", "aarch64") => 183,
        _ => fail(format!(
            "the agent library does not run on {target_arch} {target_os}"
        )),
    };
    let mut header = vec![0; 4096];
    let read = File::open(path)
        .and_then(|mut file| file.read(&mut header))
        .unwrap_or_else(|error| fail(format!("cannot read {}: {error}", path.display())));
    header.truncate(read);
    let u16_at = |offset: usize| {
        header
            .get(offset..offset + 2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    };
    let machine = if target_os == "windows" {
        // The DOS header's `e_lfanew` locates the `PE\0\0` signature, which the machine follows.
        Some(&header)
            .filter(|header| header.starts_with(b"MZ"))
            .and_then(|header| header.get(0x3c..0x40))
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
            .filter(|&pe| header.get(pe..pe + 4) == Some(b"PE\0\0".as_slice()))
            .and_then(|pe| u16_at(pe + 4))
    } else {
        // A little-endian ELF file, whose `e_machine` is at offset 18.
        Some(&header)
            .filter(|header| header.starts_with(b"\x7fELF") && header.get(5) == Some(&1))
            .and_then(|_| u16_at(18))
    };
    match machine {
        Some(machine) if machine == expected => {}
        Some(machine) => fail(format!(
            "the bundle's agent library, {}, is built for machine {machine:#06x}, not for \
             {target_arch} ({expected:#06x})",
            path.display()
        )),
        None => fail(format!(
            "the bundle's agent library, {}, is not a {} library",
            path.display(),
            if target_os == "windows" {
                "PE"
            } else {
                "little-endian ELF"
            }
        )),
    }
}

/// Copies the staged bundle beside the executables that Cargo builds, replacing only the files
/// that differ, where `agent::BundledAgent::discover` finds it, and runs this script again
/// when that copy is replaced, changed, or removed.
fn install(staged: &Path, out_dir: &Path, bundle: &Bundle, started: SystemTime) {
    // `OUT_DIR` is `<profile>/build/<package>-<hash>/out`. Cargo builds executables into
    // `<profile>`, `<profile>/examples`, and `<profile>/deps`.
    let profile = Some(out_dir)
        .filter(|out_dir| out_dir.file_name().is_some_and(|name| name == "out"))
        .and_then(Path::parent)
        .and_then(Path::parent)
        .filter(|build| build.file_name().is_some_and(|name| name == "build"))
        .and_then(Path::parent);
    let Some(profile) = profile else {
        warn(format!(
            "cannot find the directory of Cargo's executables; copy {} beside the executable, \
             or set {ARTIFACTS_DIR_ENV} to it",
            staged.display()
        ));
        return;
    };
    let installed = profile.join(DIRECTORY);
    // Cleanup never follows a link, not even one in place of the directory itself.
    if let Some(kind) = entry_type(&installed)
        && !kind.is_dir()
    {
        remove_entry(&installed, kind);
    }
    create_dir(&installed);
    // `SHA256SUMS` lists every other file of a bundle with its digest, so its own digest
    // identifies the bundle.
    let identity = sha256(&staged.join(CHECKSUMS));
    let listing = inside(&installed, CHECKSUMS);
    let replaced =
        !(entry_type(&listing).is_some_and(|kind| kind.is_file()) && sha256(&listing) == identity);
    remove_unlisted(&installed, &installed, &bundle.files);
    let mut files: Vec<(&str, String)> = bundle
        .files
        .iter()
        .map(|(name, digest)| (name.as_str(), digest.clone()))
        .collect();
    files.push((CHECKSUMS, identity.clone()));
    // Cargo runs this script again when a file that it watches is newer than the script's last
    // start, which every file written now would be. Dating the installed files a minute
    // before this run keeps its own writes from running it again.
    let dated = started
        .checked_sub(Duration::from_secs(60))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    for (name, digest) in &files {
        let target = inside(&installed, name);
        if entry_type(&target).is_some_and(|kind| kind.is_file()) && sha256(&target) == *digest {
            continue;
        }
        let partial = PathBuf::from(format!("{}.partial", target.display()));
        copy(&inside(staged, name), &partial);
        if let Err(error) = set_modified(&partial, dated) {
            warn(format!(
                "cannot date {}: {error}; the next build runs this script once more",
                partial.display()
            ));
        }
        if let Err(error) = fs::rename(&partial, &target) {
            if let Err(cleanup) = fs::remove_file(&partial) {
                warn(format!("cannot remove {}: {cleanup}", partial.display()));
            }
            fail(format!(
                "cannot replace {}: {error}; stop the processes that use it",
                target.display()
            ));
        }
    }
    // A changed or removed file of the copy runs this script again, which restores it.
    for (name, _) in &files {
        println!(
            "cargo::rerun-if-changed={}",
            inside(&installed, name).display()
        );
    }
    // Cargo caches the output of this script separately for each feature configuration of the
    // crate, and the configurations may stage different bundles. Each records the bundle that
    // it installed, and replacing that bundle makes the records of the others newer than their
    // scripts' last runs, so that their next builds restore their bundles.
    if replaced {
        outdate_other_records(&profile.join("build"), out_dir, &identity);
    }
    let record = out_dir.join(RECORD);
    // An unchanged record keeps its time, which only another configuration's installation sets
    // after this run.
    if write_if_changed(&record, &identity)
        && let Err(error) = set_modified(&record, SystemTime::UNIX_EPOCH)
    {
        warn(format!(
            "cannot date {}: {error}; the next build runs this script once more",
            record.display()
        ));
    }
    println!("cargo::rerun-if-changed={}", record.display());
}

/// Marks the records of this crate's other feature configurations that installed a bundle
/// other than `identity` as changed now, after their scripts' last runs, so that their next
/// builds run their scripts again. `builds` holds the build directories of the profile's
/// packages, one of which is `OUT_DIR`'s parent.
fn outdate_other_records(builds: &Path, out_dir: &Path, identity: &str) {
    // A configuration that misses the mark keeps this bundle, which its executables then reject.
    let missed = |what: String| {
        warn(format!(
            "{what}; other feature configurations may keep this bundle until their next change"
        ));
    };
    let own = out_dir.join(RECORD);
    let prefix = format!("{}-", env!("CARGO_PKG_NAME"));
    let now = SystemTime::now();
    let entries = match fs::read_dir(builds) {
        Ok(entries) => entries,
        Err(error) => {
            missed(format!("cannot list {}: {error}", builds.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                missed(format!("cannot list {}: {error}", builds.display()));
                continue;
            }
        };
        let record = entry.path().join("out").join(RECORD);
        if record == own || !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        match fs::read_to_string(&record) {
            Ok(other) if other != identity => {
                if let Err(error) = set_modified(&record, now) {
                    missed(format!(
                        "cannot mark {} as outdated: {error}",
                        record.display()
                    ));
                }
            }
            Ok(_) => {}
            // Configurations without `agent-bundled` and the script's own build have no record.
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => missed(format!("cannot read {}: {error}", record.display())),
        }
    }
}

fn set_modified(path: &Path, time: SystemTime) -> std::io::Result<()> {
    File::options().write(true).open(path)?.set_modified(time)
}

/// Fails if `directory` holds a link at any depth: a bundle holds only files and directories.
fn reject_links(directory: &Path) {
    for (path, kind) in entries(directory) {
        if kind.is_symlink() {
            fail(format!(
                "{} is a link; a bundle holds only files and directories",
                path.display()
            ));
        }
        if kind.is_dir() {
            reject_links(&path);
        }
    }
}

/// Removes the files below `directory` that neither `files` nor `SHA256SUMS` names, relative to
/// `root`, every link, and the directories that this leaves empty. Links are never followed.
fn remove_unlisted(root: &Path, directory: &Path, files: &BTreeMap<String, String>) {
    for (path, kind) in entries(directory) {
        // Neither a link to a directory nor a junction is a directory here.
        if kind.is_dir() {
            remove_unlisted(root, &path, files);
            // An empty directory that stays changes neither the bundle nor its discovery.
            if entries(&path).is_empty()
                && let Err(error) = fs::remove_dir(&path)
            {
                warn(format!(
                    "cannot remove the empty directory {}: {error}",
                    path.display()
                ));
            }
            continue;
        }
        let name = path
            .strip_prefix(root)
            .expect("entries are below the root")
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if kind.is_symlink() || (name != CHECKSUMS && !files.contains_key(&name)) {
            remove_entry(&path, kind);
        }
    }
}

/// Removes the file or link at `path`, which is not a directory and whose type is `kind`. A link
/// is removed, never what it points to.
fn remove_entry(path: &Path, kind: fs::FileType) {
    let removed = if is_directory_link(kind) {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    };
    removed.unwrap_or_else(|error| fail(format!("cannot remove {}: {error}", path.display())));
}

/// Returns whether `kind` is a link that Windows removes as a directory: a symbolic link to a
/// directory or a junction.
#[cfg(windows)]
fn is_directory_link(kind: fs::FileType) -> bool {
    use std::os::windows::fs::FileTypeExt;
    kind.is_symlink_dir()
}

/// Returns whether `kind` is a link that Windows removes as a directory, which no link is here.
#[cfg(not(windows))]
fn is_directory_link(_kind: fs::FileType) -> bool {
    false
}

/// Returns the type of the entry at `path`, without following a link, or `None` if there is
/// none.
fn entry_type(path: &Path) -> Option<fs::FileType> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata.file_type()),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => fail(format!("cannot inspect {}: {error}", path.display())),
    }
}

/// Removes the file at `path`, if there is one.
fn remove_file_if_present(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => fail(format!("cannot remove {}: {error}", path.display())),
    }
}

/// Writes `contents` to `path` unless it already holds them, and returns whether it wrote. An
/// unchanged file keeps its time, so that Cargo rebuilds nothing that depends on it.
fn write_if_changed(path: &Path, contents: &str) -> bool {
    match fs::read_to_string(path) {
        Ok(current) if current == contents => return false,
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => warn(format!(
            "cannot read {}: {error}; writing it again",
            path.display()
        )),
    }
    fs::write(path, contents)
        .unwrap_or_else(|error| fail(format!("cannot write {}: {error}", path.display())));
    true
}

/// Returns the entries of `directory` with their types, without following links.
fn entries(directory: &Path) -> Vec<(PathBuf, fs::FileType)> {
    fn listing<T>(directory: &Path, result: std::io::Result<T>) -> T {
        result.unwrap_or_else(|error| fail(format!("cannot list {}: {error}", directory.display())))
    }
    listing(directory, fs::read_dir(directory))
        .map(|entry| {
            let entry = listing(directory, entry);
            (entry.path(), listing(directory, entry.file_type()))
        })
        .collect()
}

/// Reads a `SHA256SUMS` file: relative paths and their lowercase SHA-256 digests.
fn read_checksums(path: &Path) -> BTreeMap<String, String> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| fail(format!("cannot read {}: {error}", path.display())));
    let mut files = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let entry = line.split_once(char::is_whitespace).map(|(digest, name)| {
            (
                digest.to_ascii_lowercase(),
                name.trim().trim_start_matches('*'),
            )
        });
        match entry {
            Some((digest, name))
                if digest.len() == 64
                    && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && name != CHECKSUMS
                    && stays_inside(name)
                    && !files.contains_key(name) =>
            {
                files.insert(name.to_owned(), digest);
            }
            _ => fail(format!("malformed line in {}: {line:?}", path.display())),
        }
    }
    files
}

/// Returns whether `name` is a relative path below the bundle's root.
fn stays_inside(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\\')
        && Path::new(name)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Returns the path in `directory` of the bundle file `name`, whose components `/` separates.
fn inside(directory: &Path, name: &str) -> PathBuf {
    name.split('/')
        .fold(directory.to_path_buf(), |path, part| path.join(part))
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| fail(format!("cannot read {}: {error}", path.display())));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| fail(format!("{} is not valid JSON: {error}", path.display())))
}

/// Returns the value of an environment variable that is set and not empty.
fn variable(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(value) => Some(value).filter(|value| !value.is_empty()),
        Err(VarError::NotPresent) => None,
        Err(VarError::NotUnicode(_)) => fail(format!("{name} is not valid Unicode")),
    }
}

/// Returns a variable that Cargo sets for build scripts.
fn cargo_variable(name: &str) -> String {
    env::var(name).unwrap_or_else(|error| fail(format!("cannot read {name}: {error}")))
}

/// Returns a path that Cargo sets for build scripts.
fn cargo_path(name: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| fail(format!("Cargo did not set {name}")))
}

/// Returns the path on the build host that a `file:` URL names, or `None` for another URL.
fn local_path(url: &str) -> Option<PathBuf> {
    const SCHEME: &str = "file://";
    fn hex(digit: u8) -> Option<u8> {
        match digit {
            b'0'..=b'9' => Some(digit - b'0'),
            b'a'..=b'f' => Some(digit - b'a' + 10),
            b'A'..=b'F' => Some(digit - b'A' + 10),
            _ => None,
        }
    }
    let rest = url
        .get(..SCHEME.len())
        .filter(|scheme| scheme.eq_ignore_ascii_case(SCHEME))
        .map(|_| &url[SCHEME.len()..])?;
    // A URL without a host, or with `localhost`, names a path on the build host; `curl`
    // handles others.
    let encoded = if rest.starts_with('/') {
        rest
    } else {
        rest.strip_prefix("localhost")
            .filter(|path| path.starts_with('/'))?
    };
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut rest = encoded.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        rest = tail;
        if byte == b'%'
            && let [high, low, after @ ..] = tail
            && let (Some(high), Some(low)) = (hex(*high), hex(*low))
        {
            bytes.push((high << 4) | low);
            rest = after;
        } else {
            bytes.push(byte);
        }
    }
    let path = String::from_utf8(bytes)
        .unwrap_or_else(|error| fail(format!("{URL} names a path that is not UTF-8: {error}")));
    // `file:///C:/bundle.zip` names `C:/bundle.zip` on Windows.
    Some(PathBuf::from(match path.as_bytes() {
        [b'/', drive, b':', ..] if cfg!(windows) && drive.is_ascii_alphabetic() => &path[1..],
        _ => &path[..],
    }))
}

fn recreate(directory: &Path) {
    if entry_type(directory).is_some() {
        fs::remove_dir_all(directory)
            .unwrap_or_else(|error| fail(format!("cannot clear {}: {error}", directory.display())));
    }
    create_dir(directory);
}

fn create_dir(directory: &Path) {
    fs::create_dir_all(directory)
        .unwrap_or_else(|error| fail(format!("cannot create {}: {error}", directory.display())));
}

fn copy(from: &Path, to: &Path) {
    create_dir(to.parent().expect("bundle files have a parent"));
    fs::copy(from, to).unwrap_or_else(|error| {
        fail(format!(
            "cannot copy {} to {}: {error}",
            from.display(),
            to.display()
        ))
    });
}

fn succeeds(command: &mut Command) -> bool {
    probe(command).is_some_and(|output| output.status.success())
}

/// Runs a command that checks for an optional tool and returns its output, or `None` if it cannot
/// run. A missing program is the expected answer; any other failure is reported.
fn probe(command: &mut Command) -> Option<Output> {
    match command.output() {
        Ok(output) => Some(output),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            warn(format!("cannot run {command:?}: {error}"));
            None
        }
    }
}

fn execute(command: &mut Command) {
    let output = command
        .output()
        .unwrap_or_else(|error| fail(format!("cannot run {command:?}: {error}")));
    if !output.status.success() {
        fail(format!(
            "{command:?} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
}

fn sha256(path: &Path) -> String {
    let file = File::open(path)
        .unwrap_or_else(|error| fail(format!("cannot open {}: {error}", path.display())));
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = reader
            .read(&mut buffer)
            .unwrap_or_else(|error| fail(format!("cannot read {}: {error}", path.display())));
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn warn(message: String) {
    println!("cargo::warning=agent: {message}");
}

fn fail(message: String) -> ! {
    panic!("agent bundled artifacts: {message}");
}
