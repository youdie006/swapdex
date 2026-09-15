//! Black-box checks for the curl-pipe installer. Every download, binary and
//! destination is synthetic; these tests never contact a release server.

use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PRIOR: &[u8] = b"#!/bin/sh\nprintf 'prior-working\\n'\n";
const GOOD: &[u8] =
    b"#!/bin/sh\n[ \"${1-}\" = --version ] || exit 9\nprintf 'swapdex fixture 1.0\\n'\n";
const BROKEN: &[u8] = b"#!/bin/sh\n[ \"${1-}\" = --version ] || exit 9\nexit 7\n";

#[derive(Clone, Copy)]
enum ChecksumDownload {
    Normal,
    Empty,
    Missing,
    Fail,
}

#[derive(Clone, Copy)]
enum Hasher {
    Sha256sum,
    Shasum,
    Missing,
    Fail,
    Garbage,
}

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    install_dir: PathBuf,
    fake_bin: PathBuf,
    archive: PathBuf,
    checksum: PathBuf,
    archive_hash: String,
}

fn executable(path: &Path, body: &[u8]) {
    fs::write(path, body).unwrap();
    let mut mode = fs::metadata(path).unwrap().permissions();
    mode.set_mode(0o755);
    fs::set_permissions(path, mode).unwrap();
}

fn real_tool(name: &str) -> PathBuf {
    let output = Command::new("/bin/sh")
        .args(["-c", "command -v \"$1\"", "sh", name])
        .output()
        .unwrap();
    assert!(output.status.success(), "test host lacks {name}");
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

impl Fixture {
    fn new(candidate: &[u8], install_name: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let install_dir = root.path().join(install_name);
        let fake_bin = root.path().join("fixture-bin");
        let payload = root.path().join("payload");
        let archive = root.path().join("swapdex.tar.gz");
        let checksum = root.path().join("swapdex.sha256");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&install_dir).unwrap();
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(&payload).unwrap();
        executable(&payload.join("swapdex"), candidate);
        executable(&install_dir.join("swapdex"), PRIOR);

        let packed = Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&payload)
            .arg("swapdex")
            .output()
            .unwrap();
        assert!(
            packed.status.success(),
            "could not create fixture archive: {}",
            String::from_utf8_lossy(&packed.stderr)
        );
        let archive_hash = Sha256::digest(fs::read(&archive).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        fs::write(&checksum, format!("{archive_hash}  swapdex.tar.gz\n")).unwrap();

        for tool in [
            "awk", "chmod", "cp", "gzip", "install", "mkdir", "mktemp", "mv", "rm", "sed", "tar",
        ] {
            symlink(real_tool(tool), fake_bin.join(tool)).unwrap();
        }
        executable(
            &fake_bin.join("uname"),
            b"#!/bin/sh\ncase \"${1-}\" in -s) echo Linux ;; -m) echo x86_64 ;; *) exit 2 ;; esac\n",
        );
        executable(
            &fake_bin.join("curl"),
            br##"#!/bin/sh
out=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out=$2; shift 2 ;;
    *) url=$1; shift ;;
  esac
done
[ -n "$out" ] || exit 2
case "$url" in
  *.tar.gz)
    [ "${FIXTURE_ARCHIVE_DOWNLOAD-normal}" != fail ] || exit 22
    cp "$FIXTURE_ARCHIVE" "$out"
    ;;
  *.sha256)
    case "${FIXTURE_CHECKSUM_DOWNLOAD-normal}" in
      normal) cp "$FIXTURE_CHECKSUM" "$out" ;;
      empty) : > "$out" ;;
      missing) : ;;
      fail) exit 22 ;;
      *) exit 2 ;;
    esac
    ;;
  *) exit 2 ;;
esac
"##,
        );

        Self {
            _root: root,
            home,
            install_dir,
            fake_bin,
            archive,
            checksum,
            archive_hash,
        }
    }

    fn destination(&self) -> PathBuf {
        self.install_dir.join("swapdex")
    }

    fn set_checksum(&self, contents: &str) {
        fs::write(&self.checksum, contents).unwrap();
    }

    fn install_hasher(&self, hasher: Hasher) {
        let (name, body): (&str, &[u8]) = match hasher {
            Hasher::Sha256sum => (
                "sha256sum",
                b"#!/bin/sh\nprintf '%s  %s\\n' \"$FIXTURE_ARCHIVE_HASH\" \"$1\"\n",
            ),
            Hasher::Shasum => (
                "shasum",
                b"#!/bin/sh\n[ \"$1\" = -a ] && [ \"$2\" = 256 ] || exit 2\nprintf '%s  %s\\n' \"$FIXTURE_ARCHIVE_HASH\" \"$3\"\n",
            ),
            Hasher::Fail => ("sha256sum", b"#!/bin/sh\nexit 3\n"),
            Hasher::Garbage => (
                "sha256sum",
                b"#!/bin/sh\nprintf 'not-a-checksum  %s\\n' \"$1\"\n",
            ),
            Hasher::Missing => return,
        };
        executable(&self.fake_bin.join(name), body);
    }

    fn fail_destination_stage(&self) {
        fs::remove_file(self.fake_bin.join("mktemp")).unwrap();
        executable(
            &self.fake_bin.join("mktemp"),
            b"#!/bin/sh\nif [ \"${1-}\" = -d ]; then exec \"$FIXTURE_REAL_MKTEMP\" -d; fi\nexit 1\n",
        );
    }

    fn run(
        &self,
        checksum_download: ChecksumDownload,
        hasher: Hasher,
        archive_download_fails: bool,
    ) -> Output {
        self.run_with_install_dir(
            checksum_download,
            hasher,
            archive_download_fails,
            Some(&self.install_dir),
        )
    }

    fn run_with_install_dir(
        &self,
        checksum_download: ChecksumDownload,
        hasher: Hasher,
        archive_download_fails: bool,
        install_dir: Option<&Path>,
    ) -> Output {
        self.install_hasher(hasher);
        let mut command = Command::new("/bin/sh");
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", &self.fake_bin)
            .env("FIXTURE_ARCHIVE", &self.archive)
            .env("FIXTURE_CHECKSUM", &self.checksum)
            .env("FIXTURE_ARCHIVE_HASH", &self.archive_hash)
            .env("FIXTURE_REAL_MKTEMP", real_tool("mktemp"))
            .env(
                "FIXTURE_ARCHIVE_DOWNLOAD",
                if archive_download_fails {
                    "fail"
                } else {
                    "normal"
                },
            )
            .env(
                "FIXTURE_CHECKSUM_DOWNLOAD",
                match checksum_download {
                    ChecksumDownload::Normal => "normal",
                    ChecksumDownload::Empty => "empty",
                    ChecksumDownload::Missing => "missing",
                    ChecksumDownload::Fail => "fail",
                },
            );
        if let Some(install_dir) = install_dir {
            command.env("INSTALL_DIR", install_dir);
        }
        command.output().unwrap()
    }

    fn assert_prior_preserved(&self, output: &Output) {
        assert!(
            !output.status.success(),
            "unsafe installer succeeded:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(self.destination()).unwrap(), PRIOR);
        assert!(
            self.has_no_staging_files(),
            "a failed install left a staging file behind"
        );
    }

    fn has_no_staging_files(&self) -> bool {
        fs::read_dir(&self.install_dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".swapdex.")
        })
    }
}

fn assert_directory_target_is_refused(symlinked: bool) {
    let fixture = Fixture::new(GOOD, "bin");
    let destination = fixture.destination();
    fs::remove_file(&destination).unwrap();
    let target_directory = if symlinked {
        let target = fixture.home.join("existing-directory-target");
        fs::create_dir(&target).unwrap();
        symlink(&target, &destination).unwrap();
        target
    } else {
        fs::create_dir(&destination).unwrap();
        destination.clone()
    };

    let output = fixture.run(ChecksumDownload::Normal, Hasher::Sha256sum, false);
    assert!(
        !output.status.success(),
        "installer reported success for a directory target:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_dir(target_directory).unwrap().next().is_none(),
        "installer moved its staging file inside the directory target"
    );
    assert!(fixture.has_no_staging_files());
    if symlinked {
        assert!(fs::symlink_metadata(destination)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}

#[test]
fn a_candidate_that_cannot_report_its_version_never_replaces_the_prior_binary() {
    let fixture = Fixture::new(BROKEN, "bin");
    let output = fixture.run(ChecksumDownload::Normal, Hasher::Sha256sum, false);
    fixture.assert_prior_preserved(&output);
}

#[test]
fn an_empty_or_missing_checksum_never_replaces_the_prior_binary() {
    for mode in [ChecksumDownload::Empty, ChecksumDownload::Missing] {
        let fixture = Fixture::new(GOOD, "bin");
        let output = fixture.run(mode, Hasher::Sha256sum, false);
        fixture.assert_prior_preserved(&output);
    }
}

#[test]
fn a_malformed_or_wrong_checksum_never_replaces_the_prior_binary() {
    for checksum in [
        "not-a-checksum\n".to_string(),
        format!("{}\n", "b".repeat(64)),
    ] {
        let fixture = Fixture::new(GOOD, "bin");
        fixture.set_checksum(&checksum);
        let output = fixture.run(ChecksumDownload::Normal, Hasher::Sha256sum, false);
        fixture.assert_prior_preserved(&output);
    }
}

#[test]
fn a_missing_or_failed_checksum_tool_never_replaces_the_prior_binary() {
    for hasher in [Hasher::Missing, Hasher::Fail, Hasher::Garbage] {
        let fixture = Fixture::new(GOOD, "bin");
        let output = fixture.run(ChecksumDownload::Normal, hasher, false);
        fixture.assert_prior_preserved(&output);
    }
}

#[test]
fn a_download_failure_never_replaces_the_prior_binary() {
    for (checksum, archive_fails) in [
        (ChecksumDownload::Normal, true),
        (ChecksumDownload::Fail, false),
    ] {
        let fixture = Fixture::new(GOOD, "bin");
        let output = fixture.run(checksum, Hasher::Sha256sum, archive_fails);
        fixture.assert_prior_preserved(&output);
    }
}

#[test]
fn a_verified_executable_is_installed_with_either_supported_hasher() {
    for hasher in [Hasher::Sha256sum, Hasher::Shasum] {
        let fixture = Fixture::new(GOOD, "bin");
        let output = fixture.run(ChecksumDownload::Normal, hasher, false);
        assert!(
            output.status.success(),
            "valid install failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let version = Command::new(fixture.destination())
            .arg("--version")
            .output()
            .unwrap();
        assert!(version.status.success());
        assert_eq!(version.stdout, b"swapdex fixture 1.0\n");
    }
}

#[test]
fn a_fresh_user_installs_to_the_default_home_bin() {
    let fixture = Fixture::new(GOOD, "unrelated-custom-bin");
    let destination = fixture.home.join(".local/bin/swapdex");
    assert!(!destination.exists());
    let output =
        fixture.run_with_install_dir(ChecksumDownload::Normal, Hasher::Sha256sum, false, None);
    assert!(
        output.status.success(),
        "fresh default install failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let version = Command::new(destination).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(version.stdout, b"swapdex fixture 1.0\n");
}

#[test]
fn a_destination_stage_failure_preserves_the_prior_binary() {
    let fixture = Fixture::new(GOOD, "bin");
    fixture.fail_destination_stage();
    let output = fixture.run(ChecksumDownload::Normal, Hasher::Sha256sum, false);
    fixture.assert_prior_preserved(&output);
}

#[test]
fn a_directory_destination_is_refused_without_moving_the_stage_inside_it() {
    assert_directory_target_is_refused(false);
}

#[test]
fn a_directory_symlink_destination_is_refused_without_moving_the_stage_inside_it() {
    assert_directory_target_is_refused(true);
}

#[test]
fn every_printed_path_hint_is_sourceable_and_preserves_the_literal_directory() {
    for name in [
        "space in bin",
        "double\"quote-bin",
        "single'quote-bin",
        "dollar$bin",
    ] {
        let fixture = Fixture::new(GOOD, name);
        let output = fixture.run(ChecksumDownload::Normal, Hasher::Sha256sum, false);
        assert!(output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        let hint = stderr
            .lines()
            .find_map(|line| line.split_once(" -> ").map(|(_, command)| command))
            .expect("installer printed a PATH command");
        let sourced = Command::new("/bin/sh")
            .args(["-c", &format!("{hint}\nprintf '%s' \"$PATH\"\n")])
            .env("PATH", "/initial/bin")
            .output()
            .unwrap();
        assert!(
            sourced.status.success(),
            "hint is not valid shell for {name:?}: {hint:?}\n{}",
            String::from_utf8_lossy(&sourced.stderr)
        );
        assert_eq!(
            String::from_utf8(sourced.stdout).unwrap(),
            format!("{}:/initial/bin", fixture.install_dir.display()),
            "hint changed the literal directory: {hint}"
        );
    }
}

#[test]
fn the_custom_install_dir_example_assigns_the_variable_to_the_installer_shell() {
    let body =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh")).unwrap();
    assert!(
        body.contains("| INSTALL_DIR=/usr/local/bin sh"),
        "INSTALL_DIR must be assigned to sh on the receiving side of the pipe"
    );
    assert!(!body.contains("INSTALL_DIR=/usr/local/bin curl"));
}
