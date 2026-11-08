# Architecture

## Design principles

1. **One thread.** A BitTorrent client spends almost all its time waiting for the network. It uses
   tokio's `current_thread` runtime and a `LocalSet`: no thread pool, no `Arc`/`Mutex`, no
   cross-thread context switches. Shared state is `Rc<RefCell<..>>`.
2. **Bounded, configurable memory.** The only thing that can grow with load is the in-flight piece
   buffers, and they are capped by a *budget* (`memory_budget`). Everything else (peer queue,
   messages, metadata, DHT candidates…) has a fixed cap.
3. **No idle work.** Nothing polls: tasks wait on `Notify`, sockets or long timers. A daemon with
   only finished torrents uses no CPU.
4. **No avoidable copies.** A `piece` message is parsed without allocating (`Message<'a>` borrows
   the read buffer) and copied exactly once, into its piece buffer.
5. **Minimal dependencies.** `tokio` (only `rt, net, time, io-util, macros, sync, signal`), `libc`
   (terminal control; tokio already depends on it), `sha1` (hardware accelerated) and `fastrand`.
   Bencode, HTTP, JSON, the MCP server, the peer protocol, DHT, UDP trackers, the CLI and the errors
   are hand-written; HTTPS is delegated to the system `curl` instead of linking a TLS stack.

## Module map

```
                         ┌──────────────┐
  CLI (main.rs, cli.rs,  │  protocol.rs │  text line over a Unix socket
   tui.rs, term.rs)      └──────┬───────┘
        │                       ▼
        ▼  Command        ┌──────────┐      ┌────────────┐
  ┌────────────┐─────────▶│manager.rs│─────▶│ engine.rs  │  (one per torrent)
  │ daemon.rs  │          │ add/pause│      │ orchestrates│
  │ server +   │          │ resume/  │      └─────┬──────┘
  │ client     │          │ remove + │            │ spawn_local
  └────────────┘          │ session +│   ┌────────┼────────────────────────┐
                          │ settings │   ▼        ▼                        ▼
                          └──────────┘ discovery.rs  peer_task × N     fetch.rs
                                       (trackers+DHT) (wire protocol)  (magnet metadata)
                                         │             │
                              tracker.rs dht.rs   wire.rs pieces.rs storage.rs
                                         ▼             ▼
                                     peers.rs     metainfo.rs  bencode.rs  bitfield.rs
```

| Module | Responsibility |
|---|---|
| `bencode` | Strict decoder with a depth limit and canonical encoder. `dict_value_span` returns the exact byte range of `info` so the info-hash is computed without re-encoding. |
| `metainfo` | Parsing and **validation** of `.torrent`/info dicts: lengths, piece count, safe paths. `build_torrent_file` packages magnet metadata while preserving the hash. |
| `magnet` | URI → info-hash (hex/base32), name, trackers, peers (`x.pe`). |
| `bitfield` | Compact bitset in BitTorrent order, with an O(1) count of set bits. |
| `wire` | Handshake, messages and `FrameReader`, a **cancel-safe** frame reader (usable inside `select!`). |
| `metadata` | BEP 10 extension and `ut_metadata` (BEP 9) plus a `MetadataAssembler` that verifies the hash. |
| `pieces` | `PieceTable`: which pieces we have, which are being assembled, which block to request (see below). |
| `storage` | Positional piece writes/reads over one or many files; resume. |
| `tracker` | HTTP (own HTTP/1.1 client), HTTPS (through `curl`) and UDP (BEP 15) trackers. |
| `curl` | Runs the system `curl` for HTTPS: scheme and URL checks, `--proto` restrictions, bounded size and time, replaceable by `WISPLET_CURL` (the tests use a fake). |
| `json` | Small strict JSON reader (depth-limited) for the search answers. |
| `api` | JSON views of the daemon's data (`rows_json`, `info_json`, `settings_json`, `result_json`) with stable, unit-suffixed field names, shared by `--json` and MCP; `clean_text` neutralises untrusted names. |
| `mcp` | The MCP server (JSON-RPC over stdio): tool catalogue with JSON schemas and annotations, argument validation, one handler per tool, behind a `Host` trait. |
| `search` | The search engine: a small registry of `SourceDef`s (request builder + pure parser) - the Internet Archive and a Torznab connector to the user's own Jackett/Prowlarr -, concurrent queries, merge by info-hash, magnet enrichment, dead-torrent filter. |
| `dht` | Iterative `get_peers` lookup (BEP 5), no routing table. |
| `peers` | Bounded, de-duplicated queue of peer addresses. |
| `discovery` | Loop that feeds the queue from trackers + DHT. |
| `fetch` | Fetches and verifies a magnet's metadata. |
| `engine` | Runs **one** torrent: resume, peers, progress. |
| `manager` | Several torrents, session persistence, pause/resume/remove, the daemon **settings** (download, watch and completed folders), the **watch-folder scanner** and the **move on completion**. |
| `daemon` / `protocol` | Unix-socket control server and its protocol. |
| `cli` / `format` / `main` | Terminal interface: argument parsing, tables, subcommands. |
| `tui` | The live interface, **pure**: `Key` decoding, the `App` state machine and the frame renderer. It reaches the daemon through a `Backend` trait, so it is tested without a terminal or a daemon. Side effects outside the daemon (search, and `f` *show in Finder*, which runs `open -R` / `xdg-open`) are also `Backend` methods with a failing default, so the tests use a fake. |
| `term` | Unix terminal shell for `tui`: raw mode on the alternate screen (`libc` termios), `poll`-based input with a 500 ms refresh tick, window size, and restoration on exit/panic/signal. |
| `autostart` | macOS LaunchAgent (`com.wisplet.daemon`) writer/reader; takes the home directory as a parameter and never calls `launchctl`. |

## Download flow

```
add ──▶ magnet? ──yes──▶ fetch_metadata ──▶ save .torrent in the state dir
          │ no                │ (x.pe peers, trackers, DHT; up to 24 in parallel)
          ▼                   ▼
     Storage::create ◀────────┘      creates/prepares the files (sparse)
          │
     resume: hash only what already existed ──▶ PieceTable
          │
     ┌────┴──────────── orchestrator loop (wakes on events or every 1 s) ───────────────┐
     │  reuse finished tasks · open connections up to max_peers                         │
     │  update Status (progress, rate) · check fatal errors / completion                │
     └──────────────────────────────────────────────────────────────────────────────────┘
          │ each connection:  peer_task
          ▼
   connect → handshake → Interested → loop { process frames; fill the pipeline;
                                             wait (read | new work | deadline) }
```

### Block selection (`PieceTable::next_request`)

1. **Finish what was started**: if a piece is in progress and the peer has it, request its next
   not-yet-requested block.
2. **Start a new piece** (if it fits in the budget): the **rarest** among those the peer has, ties
   broken by lowest index (mostly sequential writes). Since we never upload, there is no need to
   randomise for swarm health.
3. **Endgame**: when every missing piece is already in progress, pending blocks are duplicated
   across peers so one slow peer cannot delay the end.

When a piece completes its SHA-1 is checked; if it matches it is written to disk and marked as
had, otherwise it is discarded and requested again. A piece being verified cannot be reassigned.

### Peer rotation and the request queue

Because the client never uploads, tit-for-tat peers choke it; seeds and optimistic-unchoke slots are
what actually deliver data. Two mechanisms keep the connection slots useful:

- **Choke rotation.** A peer that has kept us choked for `choked_limit` (45 s) is dropped *if other
  addresses are waiting in the queue*; with an empty queue it is kept, re-checked every 5 s, up to
  `CHOKED_PATIENCE` (5 min). A dropped peer that had been connected for more than 30 s is re-queued
  at the back, so the swarm is cycled through.
- **Adaptive request queue.** Each peer has a `RateMeter` (bytes per ~1 s, exponentially smoothed).
  The number of requests kept in flight is `ceil(rate * 3 s / 16 KiB)` clamped to
  `[pipeline, pipeline_max]` (10..64 by default), so a fast or distant peer is never idle waiting
  for the next request while a slow one does not hoard blocks. `--pipeline N` sets both bounds to
  N for a fixed queue. In-flight blocks always belong to pieces counted in the memory budget, so
  the bound on buffered memory is unchanged.

- **Hoarding guard.** The adaptive queue can grow on a burst of speed; if that peer then goes quiet
  while holding more than the minimum number of requests, after 5 s (`STALL_SHED`) the oldest
  `pipeline` requests are kept, the rest are sent as `Cancel` messages and released back to the
  piece table, and the peer's rate estimate is reset so its queue falls to the minimum. Without it a
  few stalled peers could occupy all the pieces the memory budget lets be in flight until the
  request timeout (seen in a real swarm as a run at 0.05 MiB/s; the integration test uses a seeder
  that is fast and then silent and checks the `Cancel`s arrive).
- **Candidate refresh.** Rotating peers uses up the candidate list, so discovery does not wait for
  the tracker's interval (up to 15 min) when it runs low: `PeerQueue::pop` signals `low` when fewer
  than 20 addresses are waiting, and `discovery::run` then runs another round (trackers + DHT) after
  at least `min_gap` (60 s) since the previous one, so trackers are not hammered. Announces now ask
  for up to 200 peers (`numwant`; trackers cap it themselves).
- **Diagnostics.** With `-v` the engine logs every 10 s how many peers are connected, how many of
  them have us unchoked, how many candidates wait and the current rate. That line is what shows
  whether a slow download is a discovery problem (few peers), a reciprocity problem (many peers,
  few unchoking us) or a network one.

### Memory control

`max_active = memory_budget / piece_length` (at least 1). A new buffer is allocated only when the
recycle *pool* is empty and fewer than `max_active` pieces are active, so **live buffers never
exceed `max_active × piece_length`** (there is a test for the invariant). Each connection adds a
~17 KiB read buffer. Measured in a real download, the physical footprint is roughly
`memory_budget + ~4 MiB` (19.7 MiB with the default 16 MiB budget; 7.5 MiB with 4 MiB).

### Robustness against hostile or faulty peers

- Maximum frame length (`FrameReader::max_len_for`), validated bitfield (length and spare bits).
- Deadlines: connect/handshake, undelivered block (30 s), silence (150 s), long choke (5 min).
- A peer that drops releases its in-flight blocks so another peer can request them.
- Unknown, duplicate, misaligned or wrongly sized blocks are ignored.
- Metadata with a wrong hash is discarded and retried with another peer.

## The daemon

```
 wisplet add X ──▶ socket alive? ──no──▶ spawns a separate `wisplet daemon`
        │                                              │
        └────────── one text line ────────────────────▶│ UnixListener (0600)
                         OK\n<lines> | ERR msg ◀───────┘
```

- **Protocol**: one request per line, arguments percent-encoded (newline injection is impossible),
  one response, then the connection closes. Requests are capped at 64 KiB; response writes have a
  timeout so a client that never reads cannot hold the connection. Verbs: `ping`, `list`,
  `info`, `add`, `pause`, `resume`, `remove`, `settings`, `set`/`unset` (`download_dir`, `watch_dir`, `move_dir`), `move`, `shutdown`.
- **`Manager`**: each torrent is an `Entry` with a shared `Status` (what `list` shows) and a task
  (`JoinHandle`). **Pausing = aborting the task**; dropping its `JoinSet` cancels all connections
  and discovery. **Resuming** restarts the engine, which resumes from disk.
- **Persistence**: `session.txt` is rewritten atomically (temp file + `rename`). A copy of the
  `.torrent` (and of metadata fetched via magnet) is stored so the metadata phase is not repeated
  after a restart. Finished torrents are restored as finished, without re-hashing.
- **Settings**: `settings.txt` (`key<TAB>percent-encoded value`, also written atomically) holds the
  download folder. At start the saved value wins over the `--download-dir` flag, which is only the
  initial default. `set download_dir` requires an absolute path (the CLI absolutises user input
  before sending it), creates the folder, and leaves the previous value untouched on failure.
  Torrents already in the session keep the folder they were added with.
- **Watch folder** (`Manager::watch_loop`, spawned by the daemon): while `watch_dir` is set it runs
  `scan_watch_dir` every 5 s (and immediately when the setting changes, via a `Notify`); while it
  is off the task just waits on that `Notify`, so there is no timer. A scan only looks at regular
  files named `*.torrent` (`symlink_metadata`, so symlinks are never followed) that are at least
  2 s old, and calls the normal `add_file`: success or `Error::Duplicate` deletes the file, any
  other error renames it to `*.rejected`; a file that can neither be deleted nor renamed is put in
  an in-memory ignore set so it is not retried or logged forever. `add_file` itself refuses files
  over 64 MiB.
- **Move on completion** (`Manager::finish`): runs inside the torrent's task right after the engine
  reports completion. The destination is the torrent's `MoveSpec` (`Inherit` → the global
  `move_dir`, `Off`, or `Dir`), resolved *at that moment*. The item moved is the first path
  component of the torrent's files (the file for single-file torrents, the folder for multi-file
  ones). `move_item` refuses an existing destination, uses `rename`, and on `CrossesDevices` copies
  to `<dest>.tdpart` → renames → deletes the original, in `spawn_blocking` so the single-threaded
  daemon stays responsive; the state is `moving` meanwhile. On success `output_dir` becomes the
  new folder (so `info`, purge and restarts agree); on failure the files stay and a `note` is
  recorded. Because the torrent is already `done` in `session.txt`, a restart calls `finish` again
  for finished torrents, which completes an interrupted move (or notices it already happened).
  `pause` ignores finished torrents, and `remove` / a new `move` are refused while `moving`.
- **A single daemon**: if the socket answers, `daemon` fails with `AddrInUse`; an orphan socket
  left by an abrupt exit is detected (no answer) and replaced.

## The terminal interface

`wisplet <subcommand>` maps each subcommand to one protocol command. With **no arguments** and
a terminal on both stdin and stdout, `main` starts the daemon if needed and runs `term::run`; with
no terminal it prints the help, so scripts never block on a prompt.

The live view is split so that almost everything is testable:

```
 stdin bytes ─▶ tui::parse_keys ─▶ App::key ──▶ Backend::call ──▶ daemon (Unix socket)
                                     │
 500 ms of silence ─▶ App::tick ─────┤   (refreshes the list; expires notices)
                                     ▼
                           App::render(width, height) ─▶ Vec<String> ─▶ term::draw
```

- **`tui::App`** owns the rows, the selection (tracked by torrent id so it follows a torrent when
  the list changes), and a mode: `Normal`, `Input` (add / folder prompt), `Confirm` (remove) or
  `Info`. `render` returns exactly `height` lines, none wider than `width`, with a compact layout
  for narrow terminals. A failed submit keeps the prompt open and shows the error on the bottom
  line so it is never hidden behind the prompt.
- **`term`** does only what needs a real terminal: `tcsetattr` raw mode (no echo, no signals, Enter
  arrives as `\r`), alternate screen, `poll` with the refresh timeout, `TIOCGWINSZ`. Every frame
  is built as one string and written once, using clear-to-end-of-line instead of clearing the
  screen, so it does not flicker. Because release builds use `panic = "abort"` (destructors do not
  run), the terminal is also restored from a panic hook and from SIGTERM/SIGHUP handlers that call
  only async-signal-safe functions.
- The idle cost is one list request to the daemon and one redraw every 500 ms.

Paths typed at a prompt go through `cli::expand_path` (surrounding quotes, `\ ` escapes, leading
`~`) and `cli::absolute` before being sent to the daemon.

## The LLM interface

```
 MCP client (Claude...) ──stdin/stdout, one JSON-RPC message per line──▶ wisplet mcp
                                                                         │ mcp::Server<Host>
                              tools/call "add_download" ...              ▼
                                                                  Host::daemon(Command)  ──▶ daemon (Unix socket)
                                                                  Host::search / fetch_torrent ──▶ curl
```

- **Transport and protocol.** `mcp::serve` reads newline-delimited JSON-RPC 2.0 from stdin
  (lines over 1 MiB are answered with an error and skipped) and writes only protocol messages to
  stdout, so a stray `println!` anywhere would corrupt the stream: diagnostics go to stderr. It
  negotiates the protocol version (2025-06-18, 2025-03-26 or 2024-11-05), answers `initialize`,
  `ping`, `tools/list` and `tools/call`, ignores notifications, and returns JSON-RPC errors for
  malformed requests. A failing *tool* is a normal result with `isError: true`, so the model sees
  the message and can recover.
- **Stateless, thin.** Each tool maps to the same `protocol::Command` the CLI sends, so the
  daemon stays the single source of truth and the terminal, the CLI and any number of MCP clients
  agree. The only memory is the last search (so "download results 1 and 3" works). The server never
  listens on a socket of its own.
- **`Host` trait.** Everything external (daemon calls with auto-start, searching, fetching a
  `.torrent`) goes through it, so the whole protocol and every tool are unit-tested with a fake
  host; `tests/mcp.rs` then drives the real binary over pipes against a real daemon.
- **Defences, because the caller is a model and the data comes from the web.** Paths must be
  absolute (a relative one would depend on the server's cwd, which the model cannot see; a test
  once created a stray folder that way, hence the rule); `remove_download` needs an explicit
  `delete_files: true`; tools carry `readOnlyHint` / `destructiveHint` annotations; names from
  torrents and search results pass through `api::clean_text` (control characters replaced, 300
  characters max) and the search reply carries a note that they are data; schemas use
  `additionalProperties: false`; the server `instructions` repeat the rules.
- **Machine-readable CLI.** `--json` uses the same `api` functions, so a script and an MCP client
  receive identical fields (`progress_percent`, `rate_bytes_per_sec`, `eta_seconds`, `in_downloads`…).
- **For the model.** `docs/LLM.md` is compiled into the binary (`wisplet guide`), listed in
  `llms.txt`, and a test checks it mentions every tool the server exposes, so the guide cannot
  drift from the catalogue.

## Start at login

`wisplet autostart on` writes `~/Library/LaunchAgents/com.wisplet.daemon.plist`, which runs
`<binary> daemon` at login (`RunAtLoad`, deliberately without `KeepAlive`, so `stop` is respected).
launchd loads that folder by itself, so no `launchctl` call is made: enabling or disabling takes
effect at the next login and never touches the daemon running now. The plist records
`$WISPLET_HOME` when set and creates the state directory first, because launchd opens the log
file before executing the daemon. `install.sh` calls it (unless `--no-autostart`) and `--uninstall`
removes it.

## Test strategy

| Level | Where | What it covers |
|---|---|---|
| Unit (265) | `src/*/tests` | JSON reader/writer, `curl` wrapper with a fake `curl`, the search sources parsed from canned answers, the whole MCP protocol and every tool against a fake host, the JSON views, Bencode (edge and hostile cases), magnet, torrent validation, protocol messages and frames, piece selection and *endgame*, multi-file storage and resume, tracker/DHT encoding, control protocol, settings persistence in the manager, path expansion, settings, watch-folder scans and file moves in the manager (including the cross-device fallback helpers), key decoding, the live interface (state machine and rendering at many terminal sizes) with a fake backend, and the LaunchAgent plist. They include local fake UDP/HTTP/DHT servers. |
| Integration (20) | `tests/download.rs` | The real engine downloads from **fake seeders** over TCP: single and multi-file, corrupt piece, dropping peer, choke/unchoke, peers with different halves, resume, magnet with multi-piece metadata, tampered metadata. |
| Daemon (15) | `tests/daemon.rs` | Control protocol, pause/resume mid-download, `--purge`, duplicates, **daemon restart**, stale socket, permissions, garbage on the socket, settings changing the default folder and surviving a restart, **watch folder** (dropped torrent added then deleted, off by default, persisted), **move on completion** (global, per torrent, multi-file folder, conflicts, restart, purge after the move). |
| Installer (13) | `tests/install.rs` | `install.sh` with a prebuilt binary in a temporary prefix and an isolated `HOME`: install, reinstall, uninstall, **migration from the old `tDownloader` name** (old daemon and login item stopped, binary removed, state moved and `session.txt` paths rewritten, never overwriting an existing new state), PATH handling (never edited without consent), the login item (created, skipped with `--no-autostart`, removed on uninstall, reported by `autostart status`), bad arguments. |
| MCP / JSON (7) | `tests/mcp.rs` | The real `wisplet mcp` over pipes against a real daemon: handshake and catalogue, on-demand daemon start and control, add / progress / pause / resume / move / remove (with and without deleting files), settings, search then download then "already downloaded", `--json` output, and `guide`/`mcp-config` (including a check that the guide mentions every tool). |
| Terminal (4) | `tests/tui_pty.rs` | The real binary on a **pseudo-terminal**: the screen is drawn on the alternate screen, redraws itself without input, reacts to keys (pause/resume, details, add/folder/remove prompts), `q` keeps the daemon, `Q` stops it, Ctrl-C and SIGTERM restore the terminal. |
| Black box (12) | `tests/cli.rs` | The real binary in separate processes: `start/add/list/info/pause/resume/remove/stop`, `get`, `config`, `config set watch-dir/move-dir`, `add --move-to`, `move`, exit codes, auto-started daemon, help without a terminal. |

`tests/common/seeder.rs` is a fake seeder with fault injection; the benchmark
(`examples/bench_seeder.rs`) reuses it from a separate process.

## Decisions and rejected alternatives

- **From scratch vs `librqbit`.** A full client includes seeding, an HTTP API, a UI and several
  runtimes/dependencies that are unused here, raising binary size and base RAM. Writing it by hand
  gives control over every buffer. (`librqbit` was not measured; this is a design argument, not a
  benchmark.)
- **No TLS library; `curl` for HTTPS.** A TLS stack would more than double the 1 MiB binary and add a large attack surface. The things that need HTTPS (HTTPS trackers and the search indexes) are rare and not on the download path, so they run the system `curl`, which every macOS has: no new dependency, no cost unless used. The price is a run-time dependency on `curl` and a process spawn per HTTPS request (on the blocking pool, bounded in size and time).
- **MCP over stdio, no HTTP server.** A model client launches the process and talks to it over pipes: no port to protect, no authentication to design, nothing exposed to the network, and the client's own permission prompts apply (the destructive hints let it ask). Streamable HTTP could be added later if remote clients are ever needed.
- **Search ships two sources and no third-party torrent site.** The Internet Archive (free and public-domain items with `.torrent` files) and a Torznab connector that points at *the user's own* indexer manager, as Sonarr and Radarr do. The registry (`SourceDef`) makes adding a source a request builder plus a pure parser; the optional `extra-sources` Cargo feature compiles `src/search_extra.rs`, a file that is not in the repository, where anyone can register their own sources (`docs/EXTENDING.md`); the project itself publishes only the mechanism. `docs/search_extra.example.rs` is a complete example that is compiled and tested with the crate, so the instructions cannot drift from the API. The Torznab URL carries an API key, so results expose it only through server-side memory (never in the printed result) and plain `http://` is accepted only for loopback addresses.
- **Search is client-side.** It runs in the CLI/TUI process, not in the daemon, so the daemon never gains outbound web access or the parsing code. The live view runs it on a thread and polls it each tick, which keeps the screen refreshing and lets Esc cancel.
- **No DHT routing table.** A lookup-only client need not maintain one or answer queries; each
  search starts from the bootstrap nodes and frees everything when done.
- **Pause by cancellation** instead of a "frozen" state: simpler engine (no intermediate states) at
  the cost of re-verifying on resume.
- **Numeric torrent IDs** instead of hashes: friendlier in a terminal.
- **A small hand-written full-screen view** instead of a TUI crate (ratatui/crossterm): it needs only `libc`, which tokio already pulls in, so the binary does not grow; the pure/terminal split keeps it testable, and the plain subcommands remain for scripts and SSH one-liners.
- **LaunchAgent without `launchctl`**: writing the plist is enough (launchd loads it at login) and means enabling autostart can never start a second daemon or kill the running one.

## Possible improvements

- Persist the bitfield to avoid re-hashing when resuming large torrents.
- Fast Extension (BEP 6), PEX and uTP to find and keep more useful peers.
- Identify and ban peers that send corrupt data.
- Global limits (connections/RAM) shared by all torrents of the daemon.
- Speed limits and file selection.
- More settings (peer limit, memory budget) editable at runtime instead of at daemon start.
- Watch folder: also accept `.magnet` / `.txt` files with a magnet link, and per-watch-folder destinations.
