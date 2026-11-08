//! Black-box tests of `install.sh` (installs a prebuilt binary into a temporary prefix).
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_wisplet");

fn script() -> String {
    format!("{}/install.sh", env!("CARGO_MANIFEST_DIR"))
}

/// Run the installer with an isolated HOME so it can never touch the real shell configuration.
fn run(home: &Path, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(script())
        .args(args)
        .env("HOME", home)
        .env("SHELL", "/bin/zsh")
        .env("WISPLET_HOME", home.join("state"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn installs_a_working_binary_and_uninstalls_it() {
    let home = tempfile::tempdir().unwrap();
    let prefix = home.path().join("bin dir"); // a path with a space
    let p = prefix.to_str().unwrap();

    let o = run(home.path(), &["--from", BIN, "--prefix", p, "--no-path"]);
    assert!(o.status.success(), "{}", text(&o));
    // autostart is on by default (macOS), and uninstalling removes it again
    let plist = home.path().join("Library/LaunchAgents/com.wisplet.daemon.plist");
    assert_eq!(plist.exists(), cfg!(target_os = "macos"), "{}", text(&o));
    let installed = prefix.join("wisplet");
    assert!(installed.is_file());
    let help = Command::new(&installed).arg("--help").output().unwrap();
    assert!(String::from_utf8_lossy(&help.stdout).contains("USAGE"));

    // reinstalling over an existing binary works and leaves no temp file behind
    assert!(run(home.path(), &["--from", BIN, "--prefix", p, "--no-path"]).status.success());
    assert!(!prefix.join("wisplet.new").exists());

    let o = run(home.path(), &["--uninstall", "--prefix", p]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(!installed.exists());
    assert!(!plist.exists(), "login item removed on uninstall");
    // uninstalling twice is harmless
    assert!(run(home.path(), &["--uninstall", "--prefix", p]).status.success());
}

#[test]
fn prints_the_path_hint_and_never_edits_rc_files_with_no_path() {
    let home = tempfile::tempdir().unwrap();
    let p = home.path().join("bin");
    let o = run(home.path(), &["--from", BIN, "--prefix", p.to_str().unwrap(), "--no-path"]);
    let out = text(&o);
    assert!(out.contains("export PATH=") && out.contains("is not on your PATH"), "{out}");
    assert!(!home.path().join(".zshrc").exists());
}

#[test]
fn yes_adds_the_path_line_exactly_once() {
    let home = tempfile::tempdir().unwrap();
    let p = home.path().join("bin");
    let args = ["--from", BIN, "--prefix", p.to_str().unwrap(), "--yes"];
    assert!(run(home.path(), &args).status.success());
    assert!(run(home.path(), &args).status.success());
    let rc = std::fs::read_to_string(home.path().join(".zshrc")).unwrap();
    assert_eq!(rc.matches("export PATH=").count(), 1, "{rc}");
    assert!(rc.contains(p.to_str().unwrap()));
}

#[test]
fn non_interactive_run_without_flags_does_not_edit_rc_files() {
    let home = tempfile::tempdir().unwrap();
    let p = home.path().join("bin");
    let o = run(home.path(), &["--from", BIN, "--prefix", p.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(!home.path().join(".zshrc").exists());
}

#[test]
fn bad_arguments_fail_clearly() {
    let home = tempfile::tempdir().unwrap();
    for args in [vec!["--bogus"], vec!["--prefix"], vec!["--from", "/definitely/missing"]] {
        let o = run(home.path(), &args);
        assert!(!o.status.success(), "{args:?}");
        assert!(text(&o).contains("error:"), "{args:?}: {}", text(&o));
    }
}

#[test]
fn no_autostart_leaves_no_login_item() {
    let home = tempfile::tempdir().unwrap();
    let p = home.path().join("bin");
    let o = run(home.path(), &["--from", BIN, "--prefix", p.to_str().unwrap(), "--no-path", "--no-autostart"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(!home.path().join("Library").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn installed_binary_reports_and_changes_autostart() {
    let home = tempfile::tempdir().unwrap();
    let p = home.path().join("bin");
    run(home.path(), &["--from", BIN, "--prefix", p.to_str().unwrap(), "--no-path"]);
    let exe = p.join("wisplet");
    let ask = |arg: &str| {
        let o = Command::new(&exe)
            .args(["autostart", arg])
            .env("HOME", home.path())
            .env("WISPLET_HOME", home.path().join("state"))
            .output()
            .unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let (ok, out) = ask("status");
    assert!(ok && out.contains("Autostart is on"), "{out}");
    // the agent launches the installed binary with the same state directory
    let plist = std::fs::read_to_string(home.path().join("Library/LaunchAgents/com.wisplet.daemon.plist")).unwrap();
    assert!(plist.contains(p.join("wisplet").to_str().unwrap()) || plist.contains("wisplet"));
    assert!(plist.contains(home.path().join("state").to_str().unwrap()));
    let (ok, out) = ask("off");
    assert!(ok && out.contains("disabled"), "{out}");
    let (_, out) = ask("status");
    assert!(out.contains("Autostart is off"), "{out}");
    let (ok, _) = ask("bogus");
    assert!(!ok);
}

#[cfg(target_os = "macos")]
#[test]
fn reinstalling_leaves_an_existing_autostart_untouched() {
    let home = tempfile::tempdir().unwrap();
    let p = home.path().join("bin");
    let args = ["--from", BIN, "--prefix", p.to_str().unwrap(), "--no-path"];
    assert!(run(home.path(), &args).status.success());

    // the user may have edited the agent; a reinstall must not rewrite it
    let plist = home.path().join("Library/LaunchAgents/com.wisplet.daemon.plist");
    let mut text = std::fs::read_to_string(&plist).unwrap();
    text.push_str("<!-- edited by the user -->\n");
    std::fs::write(&plist, &text).unwrap();

    let o = run(home.path(), &args);
    assert!(o.status.success(), "{}", text_of(&o));
    assert!(text_of(&o).contains("already enabled"), "{}", text_of(&o));
    assert_eq!(std::fs::read_to_string(&plist).unwrap(), text, "agent file was rewritten");
    // same binary path, so no warning about a different binary
    assert!(!text_of(&o).contains("points to a different binary"), "{}", text_of(&o));
}

#[cfg(target_os = "macos")]
#[test]
fn an_agent_pointing_elsewhere_is_kept_but_reported() {
    let home = tempfile::tempdir().unwrap();
    let (first, second) = (home.path().join("one"), home.path().join("two"));
    assert!(run(home.path(), &["--from", BIN, "--prefix", first.to_str().unwrap(), "--no-path"]).status.success());
    let plist = home.path().join("Library/LaunchAgents/com.wisplet.daemon.plist");
    let before = std::fs::read_to_string(&plist).unwrap();

    let o = run(home.path(), &["--from", BIN, "--prefix", second.to_str().unwrap(), "--no-path"]);
    assert!(o.status.success(), "{}", text_of(&o));
    assert!(text_of(&o).contains("already enabled") && text_of(&o).contains("points to a different binary"), "{}", text_of(&o));
    assert_eq!(std::fs::read_to_string(&plist).unwrap(), before);
}

#[cfg(target_os = "macos")]
#[test]
fn autostart_is_enabled_again_only_when_it_is_off() {
    let home = tempfile::tempdir().unwrap();
    let p = home.path().join("bin");
    let args = ["--from", BIN, "--prefix", p.to_str().unwrap(), "--no-path"];
    run(home.path(), &args);
    let plist = home.path().join("Library/LaunchAgents/com.wisplet.daemon.plist");
    std::fs::remove_file(&plist).unwrap(); // the user turned it off
    let o = run(home.path(), &args);
    assert!(o.status.success() && plist.exists(), "{}", text_of(&o));
    assert!(!text_of(&o).contains("already enabled"));
}

fn text_of(o: &Output) -> String {
    text(o)
}

// ------------------------------------------------------------------ migration from the old name

/// Like `run`, but with the default state location (no `WISPLET_HOME`), as a real user has.
fn run_default_state(home: &Path, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(script())
        .args(args)
        .env("HOME", home)
        .env("SHELL", "/bin/zsh")
        .env_remove("XDG_DATA_HOME")
        .env_remove("WISPLET_HOME")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// A stand-in for the old `tDownloader` binary: records what it was asked to do.
fn old_binary(dir: &Path, log: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("tDownloader");
    fs::write(&path, format!("#!/bin/sh\necho \"$@\" >> '{}'\n", log.display())).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn an_installation_under_the_old_name_is_replaced_and_its_state_kept() {
    let home = tempfile::tempdir().unwrap();
    let prefix = home.path().join("bin");
    let log = home.path().join("old-calls.txt");
    let old = old_binary(&prefix, &log);
    let agents = home.path().join("Library/LaunchAgents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(agents.join("com.tdownloader.daemon.plist"), "old agent").unwrap();
    // old state: a session whose stored .torrent path points into the old state directory
    let old_state = home.path().join(".local/share/tdownloader");
    fs::create_dir_all(old_state.join("torrents")).unwrap();
    fs::write(old_state.join("torrents/abc.torrent"), "t").unwrap();
    let encoded_old = home.path().join(".local/share/tdownloader/torrents/abc.torrent").display().to_string().replace('/', "%2F");
    fs::write(old_state.join("session.txt"), format!("1\tdone\t%2Fdl\tfile\t{encoded_old}\n")).unwrap();
    fs::write(old_state.join("settings.txt"), "download_dir\t%2Fdl\n").unwrap();

    let o = Command::new("bash")
        .arg(script())
        .args(["--from", BIN, "--prefix", prefix.to_str().unwrap(), "--no-path", "--no-autostart"])
        .env("HOME", home.path())
        .env("SHELL", "/bin/zsh")
        .env_remove("XDG_DATA_HOME")
        .env_remove("WISPLET_HOME")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("Found a previous installation") && out.contains("Moved your torrents and settings"), "{out}");

    assert!(!old.exists(), "the old binary is gone");
    assert!(prefix.join("wisplet").is_file(), "the new one is installed");
    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.contains("autostart off") && calls.contains("stop"), "the old login item and daemon were stopped first: {calls}");
    assert!(!agents.join("com.tdownloader.daemon.plist").exists(), "its login item file is removed");

    let new_state = home.path().join(".local/share/wisplet");
    assert!(!old_state.exists() && new_state.join("torrents/abc.torrent").is_file(), "state moved with its contents");
    let session = fs::read_to_string(new_state.join("session.txt")).unwrap();
    assert!(session.contains("share%2Fwisplet%2Ftorrents%2Fabc.torrent") && !session.contains("tdownloader"), "paths inside the session follow the move: {session}");
    assert_eq!(fs::read_to_string(new_state.join("settings.txt")).unwrap(), "download_dir\t%2Fdl\n");
}

#[test]
fn existing_new_state_is_never_overwritten_by_the_migration() {
    let home = tempfile::tempdir().unwrap();
    let prefix = home.path().join("bin");
    let log = home.path().join("calls.txt");
    old_binary(&prefix, &log);
    let (old_state, new_state) = (home.path().join(".local/share/tdownloader"), home.path().join(".local/share/wisplet"));
    fs::create_dir_all(&old_state).unwrap();
    fs::create_dir_all(&new_state).unwrap();
    fs::write(old_state.join("session.txt"), "old").unwrap();
    fs::write(new_state.join("session.txt"), "new").unwrap();
    let o = run_default_state(home.path(), &["--from", BIN, "--prefix", prefix.to_str().unwrap(), "--no-path", "--no-autostart"]);
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(fs::read_to_string(new_state.join("session.txt")).unwrap(), "new");
    assert_eq!(fs::read_to_string(old_state.join("session.txt")).unwrap(), "old", "left where it was for the user to decide");
}

#[test]
fn nothing_to_migrate_means_no_noise() {
    let home = tempfile::tempdir().unwrap();
    let p = home.path().join("bin");
    let o = run_default_state(home.path(), &["--from", BIN, "--prefix", p.to_str().unwrap(), "--no-path", "--no-autostart"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(!text(&o).contains("previous installation"), "{}", text(&o));
}

// ------------------------------------------------------------------ the build path (fake cargo)

/// A project directory containing a copy of `install.sh` and a fake `cargo` that records its
/// arguments and "builds" by copying the real test binary into `target/release/`.
fn fake_project(with_extra_file: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let proj = tempfile::tempdir().unwrap();
    fs::copy(script(), proj.path().join("install.sh")).unwrap();
    fs::create_dir_all(proj.path().join("src")).unwrap();
    if with_extra_file {
        fs::write(proj.path().join("src/search_extra.rs"), "// local sources").unwrap();
    }
    let bin_dir = proj.path().join("fakebin");
    fs::create_dir_all(&bin_dir).unwrap();
    let log = proj.path().join("cargo-args.txt");
    let cargo = bin_dir.join("cargo");
    fs::write(
        &cargo,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{log}'\nmkdir -p target/release && cp '{bin}' target/release/wisplet\n",
            log = log.display(),
            bin = BIN
        ),
    )
    .unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    (proj, bin_dir, log)
}

fn run_build(proj: &Path, bin_dir: &Path, args: &[&str]) -> Output {
    let home = proj.join("home");
    fs::create_dir_all(&home).unwrap();
    Command::new("bash")
        .arg(proj.join("install.sh"))
        .args(args)
        .args(["--prefix", home.join("bin").to_str().unwrap(), "--no-path", "--no-autostart"])
        .env("HOME", &home)
        .env("SHELL", "/bin/zsh")
        .env("PATH", format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap()))
        .env_remove("WISPLET_HOME")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn features_are_passed_to_the_build() {
    let (proj, bin_dir, log) = fake_project(true);
    let o = run_build(proj.path(), &bin_dir, &["--features", "extra-sources"]);
    assert!(o.status.success(), "{}", text(&o));
    let args = fs::read_to_string(&log).unwrap();
    assert!(args.contains("build --release --bin wisplet --features extra-sources"), "{args}");
    assert!(!text(&o).contains("is NOT being compiled in"), "no warning when the feature was requested: {}", text(&o));
}

#[test]
fn a_plain_build_warns_when_the_local_sources_file_would_be_left_out() {
    let (proj, bin_dir, log) = fake_project(true);
    let o = run_build(proj.path(), &bin_dir, &[]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("src/search_extra.rs exists but is NOT being compiled in") && text(&o).contains("--features extra-sources"), "{}", text(&o));
    assert!(!fs::read_to_string(&log).unwrap().contains("--features"), "the build itself is unchanged");
}

#[test]
fn a_plain_build_without_the_local_file_stays_quiet() {
    let (proj, bin_dir, _log) = fake_project(false);
    let o = run_build(proj.path(), &bin_dir, &[]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(!text(&o).contains("search_extra"), "{}", text(&o));
}
