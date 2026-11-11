//! Build libghostty-vt from Ghostty's source with Zig and link it statically.
//!
//! Source lookup order:
//!
//! 1. `GHOSTTY_SOURCE_DIR`: a Ghostty checkout or extracted source tarball.
//! 2. The `third_party/ghostty` git submodule at the repository root.
//! 3. A download of `GHOSTTY_SOURCE_URL` (default: the URL in `GHOSTTY.lock`)
//!    verified against the sha256 in `GHOSTTY.lock`.
//! 4. With the `pkg-config` feature, an installed `libghostty-vt-static`.
//!
//! The source tree is copied into `$OUT_DIR/src` before Zig runs so that Zig
//! never writes into the submodule or the registry directory.
//!
//! Agentloft: a build of the pinned tarball keeps only the static archive and
//! the headers (`$OUT_DIR/ghostty-vt`, about 13 MB; the source tree and Zig's
//! cache it was built from are some 600 MB) and is shared between every
//! checkout, worktree and Cargo profile on the machine: see
//! [`shared_cache_dir`].
//!
//! Adapted from libghostty-vt-sys 0.2.2 (MIT OR Apache-2.0).

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// Pinned Ghostty source, parsed from `GHOSTTY.lock`.
struct Lock {
    commit: String,
    version: String,
    url: String,
    sha256: String,
    root: String,
}

impl Lock {
    fn read(path: &Path) -> Self {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let field = |name: &str| -> String {
            text.lines()
                .filter_map(|line| line.split_once('='))
                .find(|(key, _)| key.trim() == name)
                .map(|(_, value)| value.trim().to_owned())
                .unwrap_or_else(|| panic!("{} has no `{name}` field", path.display()))
        };
        Self {
            commit: field("commit"),
            version: field("version"),
            url: field("url"),
            sha256: field("sha256"),
            root: field("root"),
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=GHOSTTY.lock");
    for knob in [
        "DOCS_RS",
        "GHOSTTY_SOURCE_DIR",
        "GHOSTTY_SOURCE_URL",
        "GHOSTTY_ZIG_SYSTEM_DIR",
        "GHOSTTY_VT_CPU",
        "ZIG",
    ] {
        println!("cargo:rerun-if-env-changed={knob}");
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.join("..").join("..");
    let submodule = repo_root.join("third_party").join("ghostty");
    // The gitlink file changes when the submodule moves to another commit.
    // Never watch the source files themselves: a checkout touches mtimes.
    // Only watch it if it exists: Cargo counts a missing watched file as
    // changed, which would rerun this script (and rebuild everything that
    // links libghostty-vt) on every build in a checkout without the submodule.
    let gitlink = submodule.join(".git");
    if gitlink.exists() {
        println!("cargo:rerun-if-changed={}", gitlink.display());
        if let Some(head) = submodule_head(&gitlink) {
            println!("cargo:rerun-if-changed={}", head.display());
        }
    }

    let lock = Lock::read(&manifest_dir.join("GHOSTTY.lock"));
    validate_package_version(&lock);
    println!("cargo:rustc-env=GHOSTTY_VT_COMMIT={}", lock.commit);
    println!("cargo:rustc-env=GHOSTTY_VT_VERSION={}", lock.version);
    println!("cargo::metadata=COMMIT={}", lock.commit);
    println!("cargo::metadata=VERSION={}", lock.version);

    // docs.rs has no Zig toolchain. The committed bindings are enough to
    // build the documentation.
    if env::var_os("DOCS_RS").is_some() {
        return;
    }

    let target = env::var("TARGET").expect("TARGET");

    if let Some(dir) = env::var_os("GHOSTTY_SOURCE_DIR") {
        let dir = PathBuf::from(dir);
        assert!(
            dir.join("build.zig").exists(),
            "GHOSTTY_SOURCE_DIR does not contain build.zig: {}",
            dir.display()
        );
        build_from(&dir, &lock, &target);
        return;
    }

    if submodule.join("build.zig").exists() {
        build_from(&submodule, &lock, &target);
        return;
    }

    #[cfg(feature = "pkg-config")]
    if try_pkg_config(&target) {
        return;
    }

    // Agentloft: the pinned tarball always builds to the same library, so one
    // build serves every `$OUT_DIR` that asks for it.
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let install = out_dir.join("ghostty-vt");
    let key = build_key(&lock, &target);
    if !is_installed(&install, &key, &target) && !restore_shared(&install, &key, &target) {
        let src = out_dir.join("src");
        download_and_extract(&lock, &src);
        let built = build_tree(&src, &lock, &target);
        install_build(&built, &install, &key, &target);
        publish_shared(&install, &key);
    }
    // What a build is made from is not needed again (a rerun finds the
    // installed library above), nor is what an older version of this script
    // left in the same `$OUT_DIR`.
    for dir in ["src", "zig-cache", "zig-out"] {
        let _ = fs::remove_dir_all(out_dir.join(dir));
    }
    emit_link(&install.join("lib"), &install.join("include"), &target);
}

/// Agentloft: names one build of the pinned source: what goes in decides
/// what comes out. Readable, so the shared folder can be looked through.
fn build_key(lock: &Lock, target: &str) -> String {
    let cpu = env::var("GHOSTTY_VT_CPU").unwrap_or_else(|_| "baseline".to_owned());
    let key = format!(
        "{}-{}-{cpu}-r{BUILD_RECIPE}",
        lock.sha256.get(..16).unwrap_or(&lock.sha256),
        zig_target(target),
    );
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Agentloft: bump when [`build_tree`] builds differently (flags, what is
/// installed), so that builds made the old way are not reused.
const BUILD_RECIPE: u32 = 1;

/// Agentloft: where finished builds are kept for the next `$OUT_DIR` that
/// needs one: a per-user cache folder. `GHOSTTY_VT_CACHE_DIR` moves it; set
/// it to nothing to go without. Losing the folder only costs a rebuild.
fn shared_cache_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("GHOSTTY_VT_CACHE_DIR") {
        return (!dir.is_empty()).then(|| PathBuf::from(dir));
    }
    let home = env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let caches = if cfg!(target_os = "macos") {
        home.map(|home| home.join("Library").join("Caches"))
    } else if cfg!(windows) {
        env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        env::var_os("XDG_CACHE_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join(".cache")))
    };
    caches.map(|dir| dir.join("ghostty-vt-sys"))
}

fn archive_name(target: &str) -> String {
    let lib_name = lib_name(target);
    if target.contains("windows") {
        format!("{lib_name}.lib")
    } else {
        format!("lib{lib_name}.a")
    }
}

fn lib_name(target: &str) -> &'static str {
    if target.ends_with("windows-msvc") {
        "ghostty-vt-static"
    } else {
        "ghostty-vt"
    }
}

/// Whether `dir` holds a whole build (archive and headers) made for `key`.
fn is_installed(dir: &Path, key: &str, target: &str) -> bool {
    fs::read_to_string(dir.join(".key")).is_ok_and(|found| found.trim() == key)
        && dir.join("lib").join(archive_name(target)).is_file()
        && dir.join("include").join("ghostty").join("vt.h").is_file()
}

/// Copy the archive and the headers of a build into `install` and mark it
/// with `key`. The key goes in last: a copy cut short is not a build.
fn install_build(built: &Built, install: &Path, key: &str, target: &str) {
    let _ = fs::remove_dir_all(install);
    let lib = install.join("lib");
    fs::create_dir_all(&lib)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", lib.display()));
    let archive = lib.join(archive_name(target));
    fs::copy(&built.archive, &archive).unwrap_or_else(|error| {
        panic!(
            "failed to copy {} to {}: {error}",
            built.archive.display(),
            archive.display()
        )
    });
    copy_tree(&built.include_dir, &install.join("include"));
    fs::write(install.join(".key"), key)
        .unwrap_or_else(|error| panic!("failed to mark {}: {error}", install.display()));
}

/// Take the build for `key` from the shared folder, if it has one.
fn restore_shared(install: &Path, key: &str, target: &str) -> bool {
    let Some(shared) = shared_cache_dir().map(|dir| dir.join(key)) else {
        return false;
    };
    if !is_installed(&shared, key, target) {
        return false;
    }
    let built = Built {
        archive: shared.join("lib").join(archive_name(target)),
        include_dir: shared.join("include"),
    };
    install_build(&built, install, key, target);
    true
}

/// Leave the build for `key` in the shared folder. Best effort: a folder
/// that can't be written only means the next `$OUT_DIR` builds for itself.
/// The build appears under its name in one step (a rename), so another
/// build running at the same time never reads half of it.
fn publish_shared(install: &Path, key: &str) {
    let Some(cache) = shared_cache_dir() else {
        return;
    };
    let shared = cache.join(key);
    if shared.exists() {
        return;
    }
    let staging = cache.join(format!(".{key}.{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    // The key file is copied with the rest: the copy is whole.
    let published = try_copy_tree(install, &staging).and_then(|()| fs::rename(&staging, &shared));
    if published.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
}

fn try_copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            try_copy_tree(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

fn emit_link(lib_dir: &Path, include_dir: &Path, target: &str) {
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static={}", lib_name(target));
    println!("cargo:include={}", include_dir.display());
}

/// What `zig build` left behind that the crate needs.
struct Built {
    archive: PathBuf,
    include_dir: PathBuf,
}

/// Check that the published crate version identifies the pinned Ghostty source.
fn validate_package_version(lock: &Lock) {
    let short_commit = lock
        .commit
        .get(..7)
        .unwrap_or_else(|| panic!("GHOSTTY.lock has an invalid commit: {}", lock.commit));
    let (ghostty_version, version_commit) = lock.version.rsplit_once('+').unwrap_or_else(|| {
        panic!(
            "GHOSTTY.lock version must end in the short commit: {}",
            lock.version
        )
    });
    assert_eq!(
        version_commit, short_commit,
        "GHOSTTY.lock version does not match its commit"
    );

    let package_version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let metadata = package_version.split_once('+').map(|(_, value)| value);
    let expected = format!(
        "ghostty.{}.{}",
        ghostty_version.replace('+', "."),
        short_commit
    );
    assert_eq!(
        metadata,
        Some(expected.as_str()),
        "ghostty-vt-sys version must end in +{expected}"
    );
}

/// Resolve the HEAD file of the submodule's git directory, if checked out.
fn submodule_head(gitlink: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(gitlink).ok()?;
    let rel = text.strip_prefix("gitdir:")?.trim();
    let dir = gitlink.parent()?.join(rel);
    let head = dir.join("HEAD");
    head.exists().then_some(head)
}

/// Copy a source tree into `$OUT_DIR/src` and build it.
fn build_from(source: &Path, lock: &Lock, target: &str) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let src = out_dir.join("src");
    if src.exists() {
        fs::remove_dir_all(&src)
            .unwrap_or_else(|error| panic!("failed to remove {}: {error}", src.display()));
    }
    copy_tree(source, &src);
    // A local source tree changes under us: it is built where it always was
    // (Zig's cache stays, so a rerun is quick) and never shared.
    let built = build_tree(&src, lock, target);
    let lib_dir = built.archive.parent().expect("archive directory");
    emit_link(lib_dir, &built.include_dir, target);
}

/// Copy a Ghostty tree without git metadata and Zig outputs.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", to.display()));
    let entries = fs::read_dir(from)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", from.display()));
    for entry in entries {
        let entry =
            entry.unwrap_or_else(|error| panic!("failed to read {}: {error}", from.display()));
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some(".git" | ".zig-cache" | "zig-cache" | "zig-out" | "zig-pkg")
        ) {
            continue;
        }
        let path = entry.path();
        let dest = to.join(&name);
        let kind = entry
            .file_type()
            .unwrap_or_else(|error| panic!("failed to stat {}: {error}", path.display()));
        if kind.is_dir() {
            copy_tree(&path, &dest);
        } else if kind.is_file() {
            fs::copy(&path, &dest).unwrap_or_else(|error| {
                panic!(
                    "failed to copy {} to {}: {error}",
                    path.display(),
                    dest.display()
                )
            });
        }
    }
}

/// Download the pinned tarball, verify it and extract it into `src`.
fn download_and_extract(lock: &Lock, src: &Path) {
    let url = env::var("GHOSTTY_SOURCE_URL").unwrap_or_else(|_| lock.url.clone());
    let stamp = src.join(".ghostty-vt-sha256");
    if fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == lock.sha256)
        && src.join("build.zig").exists()
    {
        return;
    }
    if src.exists() {
        fs::remove_dir_all(src)
            .unwrap_or_else(|error| panic!("failed to remove {}: {error}", src.display()));
    }

    let bytes = match fetch(&url) {
        Ok(bytes) => bytes,
        Err(error) => panic!("{}", download_failure(lock, &url, &error)),
    };
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != lock.sha256 {
        panic!(
            "{}",
            download_failure(lock, &url, &format!("sha256 mismatch: got {actual}"))
        );
    }

    let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .unwrap_or_else(|error| panic!("failed to read tarball: {error}"));
    fs::create_dir_all(src)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", src.display()));
    let root = Path::new(&lock.root);
    for entry in entries {
        let mut entry =
            entry.unwrap_or_else(|error| panic!("failed to read tarball entry: {error}"));
        // git archive writes a pax global header carrying the commit id.
        let kind = entry.header().entry_type();
        if matches!(
            kind,
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader
        ) {
            continue;
        }
        let path = entry
            .path()
            .unwrap_or_else(|error| panic!("failed to read tarball entry path: {error}"))
            .into_owned();
        let Ok(rel) = path.strip_prefix(root) else {
            panic!(
                "tarball entry {} is outside the expected root {}",
                path.display(),
                root.display()
            );
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dest = src.join(rel);
        entry
            .unpack(&dest)
            .unwrap_or_else(|error| panic!("failed to extract {}: {error}", dest.display()));
    }
    fs::write(&stamp, &lock.sha256)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", stamp.display()));
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    use ureq::tls::{TlsConfig, TlsProvider};
    // Cargo.toml picks the TLS backend per build host: the OS stack on macOS
    // and Windows, rustls elsewhere. ureq still has to be told which one.
    let provider = if cfg!(any(target_os = "macos", windows)) {
        TlsProvider::NativeTls
    } else {
        TlsProvider::Rustls
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(provider).build())
        .build()
        .into();
    let response = agent.get(url).call().map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

fn download_failure(lock: &Lock, url: &str, error: &str) -> String {
    format!(
        "\n\
         ghostty-vt-sys could not obtain the Ghostty source tarball.\n\
         \n\
         error:    {error}\n\
         url:      {url}\n\
         sha256:   {sha}\n\
         commit:   {commit}\n\
         \n\
         Without network access, point GHOSTTY_SOURCE_DIR at a Ghostty checkout or\n\
         an extracted libghostty-vt source tarball at commit {commit}, or set\n\
         GHOSTTY_SOURCE_URL to a mirror of the tarball. The tarball is reproducible\n\
         from a Ghostty checkout with:\n\
         \n\
         \x20 printf '{version}' > VERSION\n\
         \x20 git archive --format=tgz --prefix={root}/ --add-file=VERSION \\\n\
         \x20   --prefix={root}/ -o libghostty-vt-source.tar.gz {commit} \\\n\
         \x20   $(for d in images macos dist/doxygen dist/linux dist/macos dist/windows \\\n\
         \x20     flatpak snap po example test src/font/res src/crash/testdata \\\n\
         \x20     pkg/wuffs/src/too_big.jpg pkg/wuffs/src/too_big.png pkg/breakpad/vendor \\\n\
         \x20     vendor; do printf ':(exclude)%s ' $d; done)\n",
        sha = lock.sha256,
        commit = lock.commit,
        version = lock.version,
        root = lock.root,
    )
}

/// Run `zig build` in `src`; the archive and the headers it made.
fn build_tree(src: &Path, lock: &Lock, target: &str) -> Built {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let zig = resolve_zig();

    let version = fs::read_to_string(src.join("VERSION"))
        .map(|v| v.trim().to_owned())
        .unwrap_or_else(|_| lock.version.clone());
    let prefix = out_dir.join("zig-out");
    let cache = out_dir.join("zig-cache");
    let cpu = env::var("GHOSTTY_VT_CPU").unwrap_or_else(|_| "baseline".to_owned());

    // Agentloft: the browser build. Ghostty's SIMD paths are C++ and need a
    // libc, which wasm32-freestanding doesn't have, and size matters more
    // than the last bit of speed in a download.
    let wasm = target.starts_with("wasm32");
    let mut build = Command::new(&zig);
    build
        .arg("build")
        .arg("-Demit-lib-vt")
        .arg(if wasm {
            "-Doptimize=ReleaseSmall"
        } else {
            "-Doptimize=ReleaseFast"
        })
        .arg(if wasm { "-Dsimd=false" } else { "-Dsimd=true" })
        .arg(format!("-Dtarget={}", zig_target(target)))
        .arg(format!("-Dcpu={cpu}"))
        .arg(format!("-Dversion-string={version}"))
        .arg("-Demit-xcframework=false")
        .arg("-Demit-themes=false")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--cache-dir")
        .arg(&cache)
        .current_dir(src);

    if let Some(dir) = env::var_os("GHOSTTY_ZIG_SYSTEM_DIR").map(PathBuf::from) {
        assert!(
            dir.is_dir(),
            "GHOSTTY_ZIG_SYSTEM_DIR is not a directory: {}",
            dir.display()
        );
        build.arg("--system").arg(&dir);
    }

    run(build, "zig build");

    let archive = prefix.join("lib").join(archive_name(target));
    assert!(
        archive.exists(),
        "zig build did not produce {}",
        archive.display()
    );
    // A wasm build installs the archive but not the headers; they are in
    // the source tree either way.
    let include_dir = if wasm {
        src.join("include")
    } else {
        prefix.join("include")
    };
    assert!(
        include_dir.join("ghostty").join("vt.h").exists(),
        "zig build did not install headers in {}",
        include_dir.display()
    );
    Built {
        archive,
        include_dir,
    }
}

fn resolve_zig() -> PathBuf {
    if let Some(zig) = env::var_os("ZIG").map(PathBuf::from) {
        check_zig_version(&zig);
        return zig;
    }

    let path_zig = PathBuf::from("zig");
    let path_result = zig_version(&path_zig);
    if matches!(&path_result, Ok(version) if version.starts_with("0.17.")) {
        return path_zig;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    if let Ok(output) = Command::new("mise")
        .args(["which", "zig"])
        .current_dir(manifest_dir)
        .output()
        && output.status.success()
    {
        let zig = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        if !zig.as_os_str().is_empty() {
            check_zig_version(&zig);
            return zig;
        }
    }

    match path_result {
        Ok(version) => panic!(
            "ghostty-vt-sys needs Zig 0.17.x, found `{version}` at `zig`.\n\
             Install it with `mise install`, activate mise, or set ZIG to the Zig 0.17 binary."
        ),
        Err(error) => panic!(
            "ghostty-vt-sys could not find Zig 0.17.x: {error}\n\
             Install it with `mise install`, activate mise, or set ZIG to the Zig 0.17 binary."
        ),
    }
}

fn check_zig_version(zig: &Path) {
    let version = zig_version(zig).unwrap_or_else(|error| panic!("{error}"));
    assert!(
        version.starts_with("0.17."),
        "ghostty-vt-sys needs Zig 0.17.x, found `{version}` at `{}`.\n\
         Install it with `mise install`, activate mise, or set ZIG to the Zig 0.17 binary.",
        zig.display()
    );
}

fn zig_version(zig: &Path) -> Result<String, String> {
    let output = Command::new(zig)
        .arg("version")
        .output()
        .map_err(|error| format!("failed to run `{} version`: {error}", zig.display()))?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() {
        return Err(format!(
            "`{} version` failed with status {}",
            zig.display(),
            output.status
        ));
    }
    Ok(version)
}

fn run(mut command: Command, context: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to execute {context}: {error}"));
    assert!(status.success(), "{context} failed with status {status}");
}

fn zig_target(target: &str) -> &'static str {
    match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "x86_64-apple-darwin" => "x86_64-macos",
        "aarch64-apple-darwin" => "aarch64-macos",
        "x86_64-pc-windows-msvc" => "x86_64-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-windows-msvc",
        "aarch64-apple-ios" => "aarch64-ios",
        "aarch64-apple-ios-sim" => "aarch64-ios-simulator",
        // Agentloft: the browser build (crates/loft-web).
        "wasm32-unknown-unknown" => "wasm32-freestanding",
        other => panic!(
            "ghostty-vt-sys has no Zig target for `{other}`; supported targets are \
             x86_64 and aarch64 linux-gnu, linux-musl, macos and windows-msvc, plus \
             aarch64-apple-ios and aarch64-apple-ios-sim"
        ),
    }
}

#[cfg(feature = "pkg-config")]
fn try_pkg_config(target: &str) -> bool {
    let Ok(lib) = pkg_config::Config::new()
        .statik(true)
        .cargo_metadata(false)
        .probe("libghostty-vt-static")
    else {
        return false;
    };
    for path in &lib.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for path in &lib.framework_paths {
        println!("cargo:rustc-link-search=framework={}", path.display());
    }
    for framework in &lib.frameworks {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=static={}", lib_name(target));
    for library in &lib.libs {
        if library != "ghostty-vt" && library != "ghostty-vt-static" {
            println!("cargo:rustc-link-lib={library}");
        }
    }
    if !lib.include_paths.is_empty() {
        let joined = env::join_paths(&lib.include_paths).expect("include paths");
        println!("cargo:include={}", joined.to_string_lossy());
    }
    true
}
