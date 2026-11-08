//! Raw terminal handling for the live interface (Unix only).
//!
//! [`RawMode`] switches the terminal to unbuffered, no-echo input on the alternate screen and
//! puts everything back on drop, on panic and on SIGTERM/SIGHUP, so a crash never leaves the
//! user's terminal unusable. [`run`] is the event loop that connects [`crate::tui::App`] to it.

use std::io::{self, Write};
use std::sync::OnceLock;
use std::time::Duration;

use crate::tui::{parse_keys, App, Backend, Flow};

static ORIGINAL: OnceLock<libc::termios> = OnceLock::new();

const ENTER: &[u8] = b"\x1b[?1049h\x1b[?25l"; // alternate screen, hide cursor
const LEAVE: &[u8] = b"\x1b[?25h\x1b[?1049l"; // show cursor, main screen

/// Undo [`RawMode::enter`]. Async-signal-safe: only `tcsetattr` and `write`.
fn restore() {
    if let Some(t) = ORIGINAL.get() {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t);
            libc::write(libc::STDOUT_FILENO, LEAVE.as_ptr().cast(), LEAVE.len());
        }
    }
}

extern "C" fn on_signal(sig: libc::c_int) {
    restore();
    unsafe { libc::_exit(128 + sig) };
}

pub struct RawMode(());

impl RawMode {
    pub fn enter() -> io::Result<RawMode> {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let _ = ORIGINAL.set(t);
        let mut raw = t;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::INLCR | libc::IGNCR);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out = io::stdout();
        out.write_all(ENTER)?;
        out.flush()?;

        unsafe {
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
        }
        // `panic = "abort"` skips destructors, so restore from the hook, before the abort.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        Ok(RawMode(()))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore();
    }
}

/// Terminal size as `(columns, rows)`; 80x24 when it cannot be read.
pub fn size() -> (usize, usize) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col as usize, ws.ws_row as usize)
    } else {
        (80, 24)
    }
}

enum Input {
    Bytes(Vec<u8>),
    Timeout,
    Eof,
}

/// Wait up to `timeout` for keyboard input.
fn read_input(timeout: Duration) -> io::Result<Input> {
    let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
    let n = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok(Input::Timeout) } else { Err(e) };
    }
    if n == 0 {
        return Ok(Input::Timeout);
    }
    let mut buf = [0u8; 512];
    let r = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
    match r {
        0 => Ok(Input::Eof),
        r if r < 0 => {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                Ok(Input::Timeout)
            } else {
                Err(e)
            }
        }
        r => Ok(Input::Bytes(buf[..r as usize].to_vec())),
    }
}

fn draw(app: &App) -> io::Result<()> {
    let (w, h) = size();
    let mut frame = String::from("\x1b[H");
    for (i, line) in app.render(w, h).iter().enumerate() {
        if i > 0 {
            frame.push('\n');
        }
        frame.push_str(line);
        frame.push_str("\x1b[K");
    }
    frame.push_str("\x1b[J");
    let mut out = io::stdout().lock();
    out.write_all(frame.as_bytes())?;
    out.flush()
}

/// How often the list refreshes while the user is idle.
pub const REFRESH: Duration = Duration::from_millis(500);

/// Run the live interface until the user quits. Returns how they quit.
pub fn run(backend: &mut dyn Backend) -> io::Result<Flow> {
    let color = std::env::var_os("NO_COLOR").is_none();
    let _raw = RawMode::enter()?;
    let mut app = App::new(color);
    app.refresh(backend);
    loop {
        draw(&app)?;
        match read_input(REFRESH)? {
            Input::Timeout => app.tick(backend),
            Input::Eof => return Ok(Flow::Quit),
            Input::Bytes(bytes) => {
                for key in parse_keys(&bytes) {
                    match app.key(key, backend) {
                        Flow::Continue => {}
                        other => return Ok(other),
                    }
                }
            }
        }
    }
}
