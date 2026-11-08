//! Start the daemon automatically when the user logs in (macOS LaunchAgent).
//!
//! `enable` writes `~/Library/LaunchAgents/com.wisplet.daemon.plist`; launchd loads every
//! agent in that folder at login, so nothing else is needed. The agent has `RunAtLoad` but no
//! `KeepAlive`: a daemon stopped with `wisplet stop` stays stopped until the next login.
//!
//! The functions take the home directory as a parameter so tests never touch the real one, and
//! none of them calls `launchctl`: enabling/disabling takes effect at the next login and never
//! kills or starts the daemon that may be running right now.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub const LABEL: &str = "com.wisplet.daemon";

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Disabled,
    /// Enabled; the binary the agent will launch.
    Enabled(PathBuf),
}

pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&apos;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// The agent definition. `state_home` is `$WISPLET_HOME` when the user set it, so the agent
/// uses the same state directory as the command line does.
pub fn render_plist(exe: &Path, log: &Path, state_home: Option<&Path>) -> String {
    let env = match state_home {
        Some(h) => format!(
            "    <key>EnvironmentVariables</key>\n    <dict>\n        <key>WISPLET_HOME</key>\n        <string>{}</string>\n    </dict>\n",
            xml_escape(&h.to_string_lossy())
        ),
        None => String::new(),
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20   <key>Label</key>\n    <string>{LABEL}</string>\n\
         \x20   <key>ProgramArguments</key>\n    <array>\n        <string>{exe}</string>\n        <string>daemon</string>\n    </array>\n\
         \x20   <key>RunAtLoad</key>\n    <true/>\n\
         \x20   <key>ProcessType</key>\n    <string>Background</string>\n\
         \x20   <key>StandardOutPath</key>\n    <string>{log}</string>\n\
         \x20   <key>StandardErrorPath</key>\n    <string>{log}</string>\n\
         {env}\
         </dict>\n\
         </plist>\n",
        exe = xml_escape(&exe.to_string_lossy()),
        log = xml_escape(&log.to_string_lossy()),
    )
}

/// Write (or update) the agent. Creates the state directory because launchd opens the log file
/// before starting the daemon.
pub fn enable(home: &Path, exe: &Path, state_dir: &Path, state_home: Option<&Path>) -> Result<PathBuf> {
    if !exe.is_absolute() {
        return Err(Error::Failed(format!("the executable path must be absolute: {}", exe.display())));
    }
    fs::create_dir_all(state_dir)?;
    let path = plist_path(home);
    fs::create_dir_all(path.parent().expect("plist has a parent"))?;
    let tmp = path.with_extension("plist.tmp");
    fs::write(&tmp, render_plist(exe, &state_dir.join("daemon.log"), state_home))?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Remove the agent. Returns `false` when it was not enabled.
pub fn disable(home: &Path) -> Result<bool> {
    match fs::remove_file(plist_path(home)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub fn status(home: &Path) -> Status {
    let Ok(text) = fs::read_to_string(plist_path(home)) else { return Status::Disabled };
    let exe = text
        .split_once("<key>ProgramArguments</key>")
        .and_then(|(_, rest)| rest.split_once("<string>"))
        .and_then(|(_, rest)| rest.split_once("</string>"))
        .map(|(v, _)| PathBuf::from(xml_unescape(v)));
    match exe {
        Some(p) => Status::Enabled(p),
        None => Status::Disabled, // unreadable file: treat as not ours
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_has_the_expected_shape() {
        let p = render_plist(Path::new("/opt/bin/wisplet"), Path::new("/s/daemon.log"), None);
        assert!(p.starts_with("<?xml"));
        assert!(p.contains("<string>com.wisplet.daemon</string>"));
        assert!(p.contains("<string>/opt/bin/wisplet</string>\n        <string>daemon</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(!p.contains("KeepAlive"), "a stopped daemon must stay stopped");
        assert!(!p.contains("EnvironmentVariables"));
        assert_eq!(p.matches("/s/daemon.log").count(), 2);
    }

    #[test]
    fn plist_passes_the_state_directory_override_and_escapes_xml() {
        let p = render_plist(Path::new("/a & b/<x>/wisplet"), Path::new("/l\"og"), Some(Path::new("/my home/it's")));
        assert!(p.contains("<key>WISPLET_HOME</key>\n        <string>/my home/it&apos;s</string>"));
        assert!(p.contains("/a &amp; b/&lt;x&gt;/wisplet"));
        assert!(p.contains("/l&quot;og"));
        assert!(!p.contains("<x>"));
    }

    #[test]
    fn enable_status_disable_roundtrip() {
        let (home, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let state = state.path().join("not yet created");
        assert_eq!(status(home.path()), Status::Disabled);
        assert!(!disable(home.path()).unwrap(), "disabling when off is harmless");

        let exe = Path::new("/Users/me/.local/bin/wisplet & co");
        let path = enable(home.path(), exe, &state, None).unwrap();
        assert_eq!(path, plist_path(home.path()));
        assert!(state.is_dir(), "state dir is created for the log file");
        assert_eq!(status(home.path()), Status::Enabled(exe.to_path_buf()));
        assert!(!path.with_extension("plist.tmp").exists());

        // enabling again updates the binary path instead of failing
        let moved = Path::new("/usr/local/bin/wisplet");
        enable(home.path(), moved, &state, None).unwrap();
        assert_eq!(status(home.path()), Status::Enabled(moved.to_path_buf()));

        assert!(disable(home.path()).unwrap());
        assert_eq!(status(home.path()), Status::Disabled);
        assert!(!plist_path(home.path()).exists());
    }

    #[test]
    fn enable_rejects_relative_executables() {
        let home = tempfile::tempdir().unwrap();
        let err = enable(home.path(), Path::new("wisplet"), home.path(), None).unwrap_err().to_string();
        assert!(err.contains("absolute"), "{err}");
        assert!(!plist_path(home.path()).exists());
    }

    #[test]
    fn status_ignores_files_that_are_not_ours() {
        let home = tempfile::tempdir().unwrap();
        let path = plist_path(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "garbage").unwrap();
        assert_eq!(status(home.path()), Status::Disabled);
    }
}
