use std::path::{Path, PathBuf};

use swapdex::service::{launchd_plist, systemd_service, unit_program};

#[test]
fn both_supervisor_formats_round_trip_a_stable_path_with_service_metacharacters() {
    let executable = Path::new("/opt/Homebrew 100% \"QA\" & tools/opt/swapdex/bin/swapdex");
    let log_dir = Path::new("/Users/Research & Development/swapdex logs");

    let systemd = systemd_service(executable, "codex");
    assert!(systemd.contains("100%% \\\"QA\\\" & tools"), "{systemd}");
    assert_eq!(unit_program(&systemd).as_deref(), executable.to_str());

    let launchd = launchd_plist(executable, "codex", log_dir);
    assert!(
        launchd.contains("100% &quot;QA&quot; &amp; tools"),
        "{launchd}"
    );
    assert_eq!(unit_program(&launchd).as_deref(), executable.to_str());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process::{Command, Output};
    use swapdex::paths::Paths;

    const CHILD_HOME: &str = "SWAPDEX_SERVICE_UPGRADE_CHILD_HOME";

    struct BrewFixture {
        _temp: tempfile::TempDir,
        prefix: PathBuf,
        home: PathBuf,
        old_executable: PathBuf,
        new_executable: PathBuf,
        linked_executable: PathBuf,
        stable_executable: PathBuf,
    }

    impl BrewFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let prefix = temp.path().join("Homebrew 100% \"QA\" & tools");
            let home = temp.path().join("isolated home");
            let old_executable = prefix.join("Cellar/swapdex/0.165.0/bin/swapdex");
            let new_executable = prefix.join("Cellar/swapdex/0.165.1/bin/swapdex");
            let linked_executable = prefix.join("bin/swapdex");
            let stable_executable = prefix.join("opt/swapdex/bin/swapdex");

            copy_current_test(&old_executable);
            copy_swapdex(&new_executable);
            fs::create_dir_all(prefix.join("bin")).unwrap();
            symlink(
                Path::new("../Cellar/swapdex/0.165.0/bin/swapdex"),
                &linked_executable,
            )
            .unwrap();
            fs::create_dir_all(prefix.join("opt")).unwrap();
            symlink(
                Path::new("../Cellar/swapdex/0.165.0"),
                prefix.join("opt/swapdex"),
            )
            .unwrap();
            fs::create_dir_all(&home).unwrap();

            Self {
                _temp: temp,
                prefix,
                home,
                old_executable,
                new_executable,
                linked_executable,
                stable_executable,
            }
        }

        fn install_from(&self, executable: &Path, path: Option<&Path>) -> (Output, String) {
            let mut command = Command::new(executable);
            command
                .args([
                    "--ignored",
                    "--exact",
                    "unix::child_installs_generated_service",
                ])
                .env(CHILD_HOME, &self.home);
            if let Some(path) = path {
                command.env("PATH", path);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "isolated service install failed; stdout: {}; stderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let unit = swapdex::service::unit_path(&Paths::rooted(&self.home), "codex");
            let body = fs::read_to_string(&unit).unwrap_or_else(|error| {
                panic!(
                    "service command did not write {}: {error}; stdout: {}; stderr: {}",
                    unit.display(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            });
            (output, body)
        }

        fn point_opt_at_new_version(&self) {
            fs::remove_file(self.prefix.join("opt/swapdex")).unwrap();
            symlink(
                Path::new("../Cellar/swapdex/0.165.1"),
                self.prefix.join("opt/swapdex"),
            )
            .unwrap();
        }
    }

    fn copy_current_test(destination: &Path) {
        copy_executable(&std::env::current_exe().unwrap(), destination);
    }

    fn copy_swapdex(destination: &Path) {
        copy_executable(Path::new(env!("CARGO_BIN_EXE_swapdex")), destination);
    }

    fn copy_executable(source: &Path, destination: &Path) {
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::copy(source, destination).unwrap();
        fs::set_permissions(destination, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn installed_program(body: &str) -> PathBuf {
        PathBuf::from(unit_program(body).expect("generated unit must name its executable"))
    }

    /// Spawned from a synthetic Cellar path so `service::install` observes that
    /// path as its own executable without touching a real user service.
    #[test]
    #[ignore = "fixture child, invoked by this integration test"]
    fn child_installs_generated_service() {
        let Some(home) = std::env::var_os(CHILD_HOME) else {
            return;
        };
        swapdex::service::install(&Paths::rooted(Path::new(&home)), "codex").unwrap();
    }

    #[test]
    fn homebrew_unit_survives_cleanup_and_tracks_the_new_cellar_version() {
        let fixture = BrewFixture::new();
        let (_output, body) = fixture.install_from(&fixture.linked_executable, None);
        let installed = installed_program(&body);

        assert_eq!(
            installed, fixture.stable_executable,
            "the service must retain Homebrew's version-independent opt path"
        );

        fs::remove_dir_all(fixture.old_executable.parent().unwrap().parent().unwrap()).unwrap();
        fixture.point_opt_at_new_version();

        assert_eq!(
            fs::canonicalize(&installed).unwrap(),
            fs::canonicalize(&fixture.new_executable).unwrap(),
            "the unchanged unit path must resolve to the upgraded binary"
        );
        let version = Command::new(&installed).arg("--version").output().unwrap();
        assert!(
            version.status.success(),
            "the generated unit path must execute after cleanup: {}",
            String::from_utf8_lossy(&version.stderr)
        );
    }

    #[test]
    fn unrelated_opt_and_path_candidates_cannot_replace_the_running_binary() {
        let fixture = BrewFixture::new();
        let decoys = fixture._temp.path().join("path shadows");
        let path_shadow = decoys.join("swapdex");
        copy_swapdex(&path_shadow);

        fs::remove_file(fixture.prefix.join("opt/swapdex")).unwrap();
        fs::create_dir_all(fixture.prefix.join("opt/swapdex/bin")).unwrap();
        copy_swapdex(&fixture.stable_executable);

        let (_output, body) = fixture.install_from(&fixture.old_executable, Some(&decoys));

        assert_eq!(
            installed_program(&body),
            fs::canonicalize(&fixture.old_executable).unwrap(),
            "a Homebrew opt candidate is safe only when it resolves to the running executable"
        );
    }

    #[test]
    fn a_non_homebrew_install_keeps_its_resolved_executable() {
        let fixture = BrewFixture::new();
        let direct = fixture._temp.path().join("standalone install/swapdex");
        copy_current_test(&direct);

        let (_output, body) = fixture.install_from(&direct, Some(&fixture.prefix.join("opt")));

        assert_eq!(installed_program(&body), fs::canonicalize(direct).unwrap());
    }

    #[test]
    fn a_missing_or_broken_opt_link_keeps_the_running_cellar_executable() {
        let fixture = BrewFixture::new();
        let opt = fixture.prefix.join("opt/swapdex");
        let running = fs::canonicalize(&fixture.old_executable).unwrap();

        fs::remove_file(&opt).unwrap();
        let (_output, missing_body) = fixture.install_from(&fixture.old_executable, None);
        assert_eq!(installed_program(&missing_body), running);

        symlink(Path::new("../Cellar/swapdex/missing"), &opt).unwrap();
        let (_output, broken_body) = fixture.install_from(&fixture.old_executable, None);
        assert_eq!(installed_program(&broken_body), running);
    }
}
