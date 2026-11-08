//! Line-based control protocol between the CLI and the daemon.
//!
//! One request line, one response, then the connection closes:
//!
//! ```text
//! request : <verb> [args...]\n          (arguments are percent-encoded)
//! response: OK\n<payload lines...>   |   ERR <message>\n
//! ```

use crate::util::{percent_decode, percent_encode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Id(u32),
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddSource {
    /// Absolute path of a `.torrent` file readable by the daemon.
    File(String),
    Magnet(String),
}

/// Where a torrent goes once it completes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MoveSpec {
    /// Use the global "move completed" folder, if one is set.
    #[default]
    Inherit,
    /// Never move this torrent, whatever the global setting is.
    Off,
    /// Move it to this folder (absolute path).
    Dir(String),
}

impl MoveSpec {
    pub fn encode(&self) -> String {
        match self {
            MoveSpec::Inherit => "default".into(),
            MoveSpec::Off => "off".into(),
            MoveSpec::Dir(d) => format!("dir:{}", enc(d)),
        }
    }

    pub fn parse(s: &str) -> Result<MoveSpec, String> {
        match s {
            "default" => Ok(MoveSpec::Inherit),
            "off" => Ok(MoveSpec::Off),
            _ => match s.strip_prefix("dir:") {
                Some(d) if !dec(d).is_empty() => Ok(MoveSpec::Dir(dec(d))),
                _ => Err(format!("invalid move setting: {}", s.chars().take(32).collect::<String>())),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Ping,
    List,
    Shutdown,
    Add { source: AddSource, dir: Option<String>, move_to: MoveSpec },
    Info(u32),
    Pause(Target),
    Resume(Target),
    Remove { id: u32, purge: bool },
    /// Show the daemon settings as `key<TAB>value` lines.
    Settings,
    SetDownloadDir(String),
    /// Watch this folder for `.torrent` files (`None` turns it off).
    SetWatchDir(Option<String>),
    /// Global folder completed torrents are moved to (`None` turns it off).
    SetMoveDir(Option<String>),
    /// Per-torrent override of the "move completed" destination.
    SetMove { id: u32, spec: MoveSpec },
}

fn enc(s: &str) -> String {
    percent_encode(s.as_bytes())
}

fn dec(s: &str) -> String {
    String::from_utf8_lossy(&percent_decode(s)).into_owned()
}

fn parse_target(s: &str) -> Result<Target, String> {
    if s == "all" {
        return Ok(Target::All);
    }
    s.parse::<u32>().map(Target::Id).map_err(|_| format!("invalid torrent id: {s}"))
}

fn parse_id(s: &str) -> Result<u32, String> {
    s.parse::<u32>().map_err(|_| format!("invalid torrent id: {s}"))
}

impl Command {
    pub fn encode(&self) -> String {
        match self {
            Command::Ping => "ping".into(),
            Command::List => "list".into(),
            Command::Shutdown => "shutdown".into(),
            Command::Add { source, dir, move_to } => {
                let (kind, value) = match source {
                    AddSource::File(p) => ("file", enc(p)),
                    AddSource::Magnet(m) => ("magnet", enc(m)),
                };
                let mut line = format!("add {kind} {value}");
                if let Some(d) = dir {
                    line.push_str(&format!(" dir={}", enc(d)));
                }
                if *move_to != MoveSpec::Inherit {
                    line.push_str(&format!(" move={}", move_to.encode()));
                }
                line
            }
            Command::Info(id) => format!("info {id}"),
            Command::Pause(t) => format!("pause {}", target_str(t)),
            Command::Resume(t) => format!("resume {}", target_str(t)),
            Command::Settings => "settings".into(),
            Command::SetDownloadDir(d) => format!("set download_dir {}", enc(d)),
            Command::SetWatchDir(Some(d)) => format!("set watch_dir {}", enc(d)),
            Command::SetWatchDir(None) => "unset watch_dir".into(),
            Command::SetMoveDir(Some(d)) => format!("set move_dir {}", enc(d)),
            Command::SetMoveDir(None) => "unset move_dir".into(),
            Command::SetMove { id, spec } => format!("move {id} {}", spec.encode()),
            Command::Remove { id, purge } => {
                if *purge {
                    format!("remove {id} purge")
                } else {
                    format!("remove {id}")
                }
            }
        }
    }

    pub fn parse(line: &str) -> Result<Command, String> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["ping"] => Ok(Command::Ping),
            ["list"] => Ok(Command::List),
            ["shutdown"] => Ok(Command::Shutdown),
            ["settings"] => Ok(Command::Settings),
            ["set", "download_dir", v] => Ok(Command::SetDownloadDir(dec(v))),
            ["set", "watch_dir", v] if !dec(v).is_empty() => Ok(Command::SetWatchDir(Some(dec(v)))),
            ["unset", "watch_dir"] => Ok(Command::SetWatchDir(None)),
            ["set", "move_dir", v] if !dec(v).is_empty() => Ok(Command::SetMoveDir(Some(dec(v)))),
            ["unset", "move_dir"] => Ok(Command::SetMoveDir(None)),
            ["move", id, spec] => Ok(Command::SetMove { id: parse_id(id)?, spec: MoveSpec::parse(spec)? }),
            ["add", kind, value, opts @ ..] => {
                let (mut dir, mut move_to) = (None, MoveSpec::Inherit);
                for o in opts {
                    match o.split_once('=') {
                        Some(("dir", d)) if !dec(d).is_empty() => dir = Some(dec(d)),
                        Some(("move", m)) => move_to = MoveSpec::parse(m)?,
                        _ => return Err(format!("unknown add option: {}", o.chars().take(32).collect::<String>())),
                    }
                }
                let source = match *kind {
                    "file" => AddSource::File(dec(value)),
                    "magnet" => AddSource::Magnet(dec(value)),
                    other => return Err(format!("unknown source kind: {other}")),
                };
                Ok(Command::Add { source, dir, move_to })
            }
            ["info", id] => Ok(Command::Info(parse_id(id)?)),
            ["pause", t] => Ok(Command::Pause(parse_target(t)?)),
            ["resume", t] => Ok(Command::Resume(parse_target(t)?)),
            ["remove", id] => Ok(Command::Remove { id: parse_id(id)?, purge: false }),
            ["remove", id, "purge"] => Ok(Command::Remove { id: parse_id(id)?, purge: true }),
            [] => Err("empty command".into()),
            [verb, ..] => Err(format!(
                "unknown or malformed command: {}",
                verb.chars().take(32).collect::<String>()
            )),
        }
    }
}

fn target_str(t: &Target) -> String {
    match t {
        Target::All => "all".into(),
        Target::Id(i) => i.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Ok(Vec<String>),
    Err(String),
}

impl Response {
    pub fn encode(&self) -> String {
        match self {
            Response::Ok(lines) => {
                let mut s = String::from("OK\n");
                for l in lines {
                    s.push_str(l);
                    s.push('\n');
                }
                s
            }
            Response::Err(m) => format!("ERR {}\n", m.replace('\n', " ")),
        }
    }

    pub fn parse(text: &str) -> Result<Response, String> {
        let mut lines = text.lines();
        match lines.next() {
            Some("OK") => Ok(Response::Ok(lines.map(str::to_string).collect())),
            Some(l) if l.starts_with("ERR") => Ok(Response::Err(l[3..].trim().to_string())),
            _ => Err("malformed response from daemon".into()),
        }
    }
}

/// One row of `list` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: u32,
    pub state: String,
    pub done: u64,
    pub total: u64,
    pub rate: u64,
    pub peers: usize,
    pub info_hash: String,
    pub name: String,
}

impl Row {
    pub fn encode(&self) -> String {
        let name: String = self.name.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.id, self.state, self.done, self.total, self.rate, self.peers, self.info_hash, name
        )
    }

    pub fn parse(line: &str) -> Option<Row> {
        let mut f = line.splitn(8, '\t');
        Some(Row {
            id: f.next()?.parse().ok()?,
            state: f.next()?.to_string(),
            done: f.next()?.parse().ok()?,
            total: f.next()?.parse().ok()?,
            rate: f.next()?.parse().ok()?,
            peers: f.next()?.parse().ok()?,
            info_hash: f.next()?.to_string(),
            name: f.next()?.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(c: Command) {
        let line = c.encode();
        assert!(!line.contains('\n'));
        assert_eq!(Command::parse(&line).unwrap(), c, "{line}");
    }

    #[test]
    fn commands_roundtrip() {
        roundtrip(Command::Ping);
        roundtrip(Command::List);
        roundtrip(Command::Shutdown);
        roundtrip(Command::Info(7));
        roundtrip(Command::Settings);
        roundtrip(Command::SetDownloadDir("/data/my downloads/ñ".into()));
        roundtrip(Command::Pause(Target::All));
        roundtrip(Command::Resume(Target::Id(3)));
        roundtrip(Command::Remove { id: 4, purge: false });
        roundtrip(Command::Remove { id: 4, purge: true });
        roundtrip(Command::Add {
            source: AddSource::Magnet("magnet:?xt=urn:btih:abc&dn=a b&tr=udp%3A%2F%2Fx".into()),
            dir: None,
            move_to: MoveSpec::Inherit,
        });
        roundtrip(Command::Add {
            source: AddSource::File("/home/user/my files/ñ.torrent".into()),
            dir: Some("/data/dl dir".into()),
            move_to: MoveSpec::Dir("/done/my films".into()),
        });
        roundtrip(Command::Add {
            source: AddSource::File("/a.torrent".into()),
            dir: None,
            move_to: MoveSpec::Off,
        });
        roundtrip(Command::SetWatchDir(Some("/watch me".into())));
        roundtrip(Command::SetWatchDir(None));
        roundtrip(Command::SetMoveDir(Some("/done/ñ".into())));
        roundtrip(Command::SetMoveDir(None));
        for spec in [MoveSpec::Inherit, MoveSpec::Off, MoveSpec::Dir("/x y".into())] {
            roundtrip(Command::SetMove { id: 9, spec });
        }
    }

    #[test]
    fn rejects_bad_commands() {
        let long = format!("{}x", "a".repeat(10_000));
        let msg = Command::parse(&long).unwrap_err();
        assert!(msg.len() < 100, "error echoes unbounded user input: {} bytes", msg.len());
        for bad in ["", "frobnicate", "pause", "pause x", "info -1", "remove 1 delete", "add torrent x", "add file", "set", "set download_dir", "set colour red", "settings now", "set watch_dir", "unset watch_dir x", "move 1", "move x off", "move 1 dir:", "move 1 sideways", "add file x bogus=1", "add file x move=nope"] {
            assert!(Command::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn hostile_arguments_cannot_inject_lines() {
        let c = Command::Add {
            source: AddSource::Magnet("x\nshutdown".into()),
            dir: Some("a b\r\nc".into()),
            move_to: MoveSpec::Dir("d\ne".into()),
        };
        let line = c.encode();
        assert!(!line.contains('\n') && !line.contains('\r'));
        assert_eq!(Command::parse(&line).unwrap(), c);
    }

    #[test]
    fn responses_roundtrip() {
        let ok = Response::Ok(vec!["a".into(), "b\tc".into()]);
        assert_eq!(Response::parse(&ok.encode()).unwrap(), ok);
        assert_eq!(Response::parse(&Response::Ok(vec![]).encode()).unwrap(), Response::Ok(vec![]));
        let err = Response::Err("boom\nsecond".into());
        assert_eq!(Response::parse(&err.encode()).unwrap(), Response::Err("boom second".into()));
        assert!(Response::parse("garbage").is_err());
        assert!(Response::parse("").is_err());
    }

    #[test]
    fn rows_roundtrip_and_sanitize() {
        let row = Row {
            id: 3,
            state: "downloading".into(),
            done: 10,
            total: 100,
            rate: 5,
            peers: 2,
            info_hash: "ab".repeat(20),
            name: "tab\there\nnewline".into(),
        };
        let parsed = Row::parse(&row.encode()).unwrap();
        assert_eq!(parsed.name, "tab here newline");
        assert_eq!(parsed.id, 3);
        assert!(Row::parse("not a row").is_none());
    }
}
