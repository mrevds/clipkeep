//! Line protocol between the popup/CLI and the daemon.
//! The client sends one command line. The daemon answers with zero or more data lines,
//! then `OK` or `ERR <reason>`. Data fields are tab-separated and never contain newlines.
//! Unix: socket in $XDG_RUNTIME_DIR (mode 0600). Windows: TCP on loopback only.

use crate::store::{Hit, View};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Ping,
    Show,
    Copy(u32),
    /// Thumbnail PNG of an image clip (reply: one `T\t<base64>` line).
    Thumb(u32),
    /// Press Ctrl+V in the focused app (after the popup closed).
    Paste,
    Pin(u32),
    Delete(u32),
    Clear,
    Quit,
    /// Sync with the other devices right now.
    Sync,
    Stats,
    Groups,
    SetGroup(u32, String),
    Search {
        view: View,
        limit: usize,
        query: String,
    },
}

fn view_token(v: View) -> String {
    match v {
        View::All => "all".into(),
        View::Pinned => "pinned".into(),
        View::Links => "links".into(),
        View::Images => "images".into(),
        View::Group(g) => format!("g{g}"),
    }
}

fn parse_view(s: &str) -> Option<View> {
    Some(match s {
        "all" => View::All,
        "pinned" => View::Pinned,
        "links" => View::Links,
        "images" => View::Images,
        _ => View::Group(s.strip_prefix('g')?.parse().ok()?),
    })
}

fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

impl Command {
    pub fn encode(&self) -> String {
        match self {
            Command::Ping => "PING".into(),
            Command::Show => "SHOW".into(),
            Command::Copy(id) => format!("COPY {id}"),
            Command::Thumb(id) => format!("THUMB {id}"),
            Command::Paste => "PASTE".into(),
            Command::Pin(id) => format!("PIN {id}"),
            Command::Delete(id) => format!("DEL {id}"),
            Command::Clear => "CLEAR".into(),
            Command::Quit => "QUIT".into(),
            Command::Sync => "SYNC".into(),
            Command::Stats => "STATS".into(),
            Command::Groups => "GROUPS".into(),
            Command::SetGroup(id, name) => format!("SETGROUP {id} {}", one_line(name)),
            Command::Search { view, limit, query } => {
                format!("SEARCH {} {limit} {}", view_token(*view), one_line(query))
            }
        }
    }

    pub fn parse(line: &str) -> Option<Command> {
        let line = line.trim_end_matches(['\n', '\r']);
        let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
        let id = || rest.trim().parse::<u32>().ok();
        Some(match name {
            "PING" => Command::Ping,
            "SHOW" => Command::Show,
            "COPY" => Command::Copy(id()?),
            "THUMB" => Command::Thumb(id()?),
            "PASTE" => Command::Paste,
            "PIN" => Command::Pin(id()?),
            "DEL" => Command::Delete(id()?),
            "CLEAR" => Command::Clear,
            "QUIT" => Command::Quit,
            "SYNC" => Command::Sync,
            "STATS" => Command::Stats,
            "GROUPS" => Command::Groups,
            "SETGROUP" => {
                let (id, name) = rest.split_once(' ').unwrap_or((rest, ""));
                Command::SetGroup(id.parse().ok()?, name.to_owned())
            }
            "SEARCH" => {
                let mut parts = rest.splitn(3, ' ');
                let view = parse_view(parts.next()?)?;
                let limit = parts.next()?.parse().ok()?;
                Command::Search {
                    view,
                    limit,
                    query: parts.next().unwrap_or("").to_owned(),
                }
            }
            _ => return None,
        })
    }
}

pub fn encode_hit(h: &Hit) -> String {
    format!(
        "R\t{}\t{}\t{}\t{}\t{}",
        h.id,
        if h.pinned { 'p' } else { '-' },
        h.group,
        h.kind,
        h.preview
    )
}

pub fn parse_hit(line: &str) -> Option<Hit> {
    let mut f = line.strip_prefix("R\t")?.splitn(5, '\t');
    Some(Hit {
        id: f.next()?.parse().ok()?,
        pinned: f.next()? == "p",
        group: f.next()?.parse().ok()?,
        kind: f.next()?.parse().ok()?,
        preview: f.next()?.to_owned(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRow {
    pub id: u16,
    pub count: usize,
    pub name: String,
}

pub fn encode_group(id: u16, count: usize, name: &str) -> String {
    format!("G\t{id}\t{count}\t{}", one_line(name))
}

pub fn parse_group(line: &str) -> Option<GroupRow> {
    let mut f = line.strip_prefix("G\t")?.splitn(3, '\t');
    Some(GroupRow {
        id: f.next()?.parse().ok()?,
        count: f.next()?.parse().ok()?,
        name: f.next()?.to_owned(),
    })
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for b in s.trim_end_matches('=').bytes() {
        acc = acc << 6 | B64.iter().position(|&x| x == b)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;
#[cfg(unix)]
pub type Listener = std::os::unix::net::UnixListener;
#[cfg(windows)]
type Stream = std::net::TcpStream;
#[cfg(windows)]
pub type Listener = std::net::TcpListener;

#[cfg(windows)]
const ADDR: &str = "127.0.0.1:47651";

fn connect() -> io::Result<Stream> {
    #[cfg(unix)]
    return Stream::connect(crate::paths::socket_file());
    #[cfg(windows)]
    return Stream::connect(ADDR);
}

/// Sends a command and returns the daemon's data lines (`Err` for `ERR` replies).
pub fn request(cmd: &Command) -> io::Result<Vec<String>> {
    let mut stream = connect()?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(format!("{}\n", cmd.encode()).as_bytes())?;
    let mut lines = Vec::new();
    for line in BufReader::new(stream).lines() {
        let line = line?;
        if line == "OK" {
            return Ok(lines);
        }
        if let Some(e) = line.strip_prefix("ERR") {
            return Err(io::Error::other(e.trim().to_owned()));
        }
        lines.push(line);
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "daemon closed the connection",
    ))
}

pub fn daemon_running() -> bool {
    request(&Command::Ping).is_ok()
}

/// Binds the daemon socket. Fails if another daemon is alive.
pub fn listen() -> io::Result<Listener> {
    if daemon_running() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "clipkeep daemon is already running",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = crate::paths::socket_file();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let _ = std::fs::remove_file(&path); // stale socket from a crashed daemon
        let listener = Listener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }
    #[cfg(windows)]
    Listener::bind(ADDR)
}

/// Serves connections one by one until a `QUIT` was handled.
pub fn serve(listener: Listener, mut handle: impl FnMut(&Command) -> Result<Vec<String>, String>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let Ok(reader) = stream.try_clone() else {
            continue;
        };
        let mut line = String::new();
        if BufReader::new(reader).read_line(&mut line).is_err() {
            continue;
        }
        let cmd = Command::parse(&line);
        let mut out = BufWriter::new(stream);
        let result = match &cmd {
            Some(cmd) => handle(cmd),
            None => Err("unknown command".into()),
        };
        let _ = match result {
            Ok(lines) => lines
                .iter()
                .try_for_each(|l| writeln!(out, "{l}"))
                .and_then(|_| writeln!(out, "OK")),
            Err(e) => writeln!(out, "ERR {}", one_line(&e)),
        };
        let _ = out.flush();
        if cmd == Some(Command::Quit) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_commands() {
        for cmd in [
            Command::Ping,
            Command::Show,
            Command::Copy(7),
            Command::Thumb(8),
            Command::Search {
                view: View::Images,
                limit: 3,
                query: String::new(),
            },
            Command::Paste,
            Command::Pin(u32::MAX),
            Command::Delete(0),
            Command::Clear,
            Command::Quit,
            Command::Sync,
            Command::Stats,
            Command::Groups,
            Command::SetGroup(3, "Работа и учёба".into()),
            Command::SetGroup(3, String::new()),
            Command::Search {
                view: View::All,
                limit: 200,
                query: "привет  мир".into(),
            },
            Command::Search {
                view: View::Group(12),
                limit: 5,
                query: String::new(),
            },
            Command::Search {
                view: View::Links,
                limit: 1,
                query: "https://x".into(),
            },
            Command::Search {
                view: View::Pinned,
                limit: 1,
                query: "a".into(),
            },
        ] {
            assert_eq!(Command::parse(&cmd.encode()), Some(cmd));
        }
    }

    #[test]
    fn base64_roundtrip() {
        for n in 0..40usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 5) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert!(base64_decode("@@@@").is_none());
    }

    #[test]
    fn newlines_cannot_break_the_protocol() {
        let cmd = Command::SetGroup(1, "a\nCLEAR".into());
        assert!(!cmd.encode().contains('\n'));
        assert_eq!(
            Command::parse(&cmd.encode()),
            Some(Command::SetGroup(1, "a CLEAR".into()))
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Command::parse(""), None);
        assert_eq!(Command::parse("COPY"), None);
        assert_eq!(Command::parse("COPY abc"), None);
        assert_eq!(Command::parse("КОПИЯ 1"), None);
        assert_eq!(Command::parse("SEARCH bogus 10 x"), None);
        assert_eq!(Command::parse("SEARCH all x"), None);
        assert_eq!(Command::parse("COPY 5\n"), Some(Command::Copy(5)));
    }

    #[test]
    fn hit_and_group_lines() {
        let h = Hit {
            id: 9,
            pinned: true,
            group: 2,
            kind: 1,
            preview: "https://пример.uz a\tb".replace('\t', " "),
        };
        assert_eq!(parse_hit(&encode_hit(&h)), Some(h));
        let g = parse_group(&encode_group(4, 17, "Ссылки\tработы")).unwrap();
        assert_eq!(
            g,
            GroupRow {
                id: 4,
                count: 17,
                name: "Ссылки работы".into()
            }
        );
        assert_eq!(parse_hit("G\t1"), None);
    }
}
