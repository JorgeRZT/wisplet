<h1 align="center">wisplet</h1>

<p align="center"><b>A download-only BitTorrent client that sips CPU and RAM</b> — with a live terminal UI<br>
and an MCP server so an LLM (Claude, …) can drive it.</p>

<p align="center">
  <a href="https://github.com/JorgeRZT/wisplet/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/JorgeRZT/wisplet/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="Rust" src="https://img.shields.io/badge/rust-stable-orange.svg?logo=rust">
  <img alt="Platform" src="https://img.shields.io/badge/platform-macOS-lightgrey.svg?logo=apple">
  <img alt="Memory" src="https://img.shields.io/badge/RAM-~20%20MiB%20downloading-brightgreen.svg">
  <img alt="Idle" src="https://img.shields.io/badge/idle-1%20MiB%20%C2%B7%200%25%20CPU-brightgreen.svg">
  <img alt="MCP" src="https://img.shields.io/badge/MCP-server-8A2BE2.svg">
  <img alt="Tests" src="https://img.shields.io/badge/tests-339-brightgreen.svg">
</p>

## Why wisplet

Most BitTorrent clients are GUI applications that also seed, serve a web UI and keep a lot in
memory. wisplet does one thing — **download** — and is built around a single idea: a hard,
configurable bound on CPU and RAM.

Measured against Transmission, three clients downloading the same 2.7 GiB torrent at the same moment
on the same swarm (Apple M4, macOS; details and caveats in [docs/PERFORMANCE.md](docs/PERFORMANCE.md)):

| | Time | CPU per GiB | Memory (peak footprint) |
|---|---|---|---|
| **wisplet** | **160 s** | **5.1 s** | **20 MiB** |
| Transmission | 223 s | 19.8 s | 91 MiB (RSS 153 MiB) |

**About 4× less CPU and 4.5× less memory**, and an idle daemon uses ~1 MiB and no CPU. (One run on
one network; Transmission has a GUI and also uploads, which accounts for part of its cost. Speed
varies a lot between runs, so it is only comparable when the clients run at the same moment.)

How it gets there: one thread and no locks (`tokio` `current_thread`), piece buffers bounded by a
memory budget and recycled, zero-copy message parsing, positional writes, hardware SHA-1, and
hand-written bencode, HTTP, JSON and MCP instead of dependencies. Four direct dependencies, a ~1.2 MiB
binary.

- **Live terminal UI** — a full-screen view that refreshes by itself: select, add, pause, remove,
  search and change settings with single keys.
- **Made for LLMs** — `wisplet mcp` is an [MCP](https://modelcontextprotocol.io) server with 13 tools
  (list, add, pause/resume/remove, search, settings…), and `--json` makes the command line
  machine-readable. Ask Claude *"how are my downloads?"* or *"add this magnet link"*. See
  [docs/LLM.md](docs/LLM.md).
- **A background daemon** you control from the terminal; it starts on demand, remembers its torrents
  across restarts and can start at login (macOS).
- **Handy automation** — a watch folder (drop a `.torrent`, it starts) and moving finished downloads
  to another folder, globally or per torrent.
- **Download-only** by design: it never uploads.
- **339 automated tests**: unit, integration against fake peers, and black-box tests of the real binary
  on a pseudo-terminal and over MCP.

**Contents:** [Installation](#installation) · [Usage](#usage) · [LLMs and MCP](#using-it-from-an-llm-claude-mcp-or-a-script) ·
[Search sources](#adding-your-own-search-sources) · [Measurements](#figures-measured-apple-m4-macos) · [Options](#options) · [Supported](#what-is-and-is-not-supported) ·
[Responsible use](#responsible-use) · [Development](#development) · [Security](#security)

## Figures (measured, Apple M4, macOS)

Memory = **physical footprint** (the private memory the process really uses, from `vmmap`). The RSS
shown by `ps`/`top` is larger because it includes ~8 MiB of shared pages from system libraries.

| Scenario | CPU | Memory (footprint) | RSS |
|---|---|---|---|
| Idle daemon (no torrents, watch folder on, or only finished torrents) | 0.00 % | **1.1-1.2 MiB** | 2.0-2.6 MiB |
| Real download, 2.7 GiB, 30 peers, defaults, 17.5 MiB/s | **5.1 s of CPU per GiB** (8.8 % of a core) | **20 MiB** | 28 MiB |
| Same download in **Transmission**, same swarm, same moment | 19.8 s of CPU per GiB (24 % of a core) | 91 MiB | 153 MiB |
| Real download with `--memory-mb 4 --max-peers 15` | 0.72 s CPU in the first minute | **7.5 MiB** | 15 MiB |
| Synthetic loopback, 512 MiB, 8 seeders, 4 MiB pieces | 4.2 s CPU per GiB (170 MiB/s) | — | 10.4 MiB |

Speed depends on the swarm and varies a lot between runs: see [docs/PERFORMANCE.md](docs/PERFORMANCE.md)
for the method, the other runs, what did not help, and the limits of these numbers. Memory grows with
`--memory-mb` (piece buffers) and is flat over time.

## Installation

Requires stable Rust (tested with 1.97; get it with [rustup](https://rustup.rs)). `curl` (included
with macOS) is needed at run time only for HTTPS trackers and for `search`.

```sh
./install.sh
```

The script builds the release binary and installs it as `wisplet` into `~/.local/bin`. If that
folder is not on your `PATH` it offers to add it to `~/.zshrc` (or `~/.bash_profile`); open a new
terminal afterwards and run `wisplet`. On macOS it also **starts the daemon automatically at
login** (see below).

| Option | Effect |
|---|---|
| `--prefix DIR` | Install into `DIR` instead of `~/.local/bin` |
| `--from BINARY` | Install an already built binary (skips the build) |
| `--yes` / `--no-path` | Add the folder to `PATH` without asking / never touch your shell configuration |
| `--no-autostart` | Do not start the daemon at login |
| `--uninstall` | Stop the daemon, remove the login item and the binary (downloads and state are kept) |

If you had the earlier version (installed under the name `tDownloader`), the script replaces it: it
stops its daemon and login item, removes the old binary and moves your torrents and settings to the
new state directory (`~/.local/share/wisplet`).

Manual alternative: `cargo build --release && cp target/release/wisplet ~/.local/bin/`.

## Usage

### Live view

Run `wisplet` with no arguments in a terminal. The daemon is started automatically if needed.
The list refreshes twice a second on its own; you never have to type a command to see progress.

```
 wisplet                                  2 torrents · 1 downloading · ↓ 3.3 MiB/s
 Download folder: /Users/you/Downloads
 Watch folder: /Users/you/Torrents/inbox · Move completed to: /Volumes/DATA/Done

  ID  STATE       PROGRESS         %       SIZE         DOWN       ETA PEERS  NAME
▶ 1   downloading [#####.....]  52.3  756.0 MiB    3.3 MiB/s     1m09s    23  debian-13.7.0-amd64-netinst.iso
  2   paused      [##........]  18.0    4.2 GiB            -         -     0  dataset.tar

 Removed torrent 3
 ↑↓ select · a add · / search · p pause · x remove · i info · s/w/m folders · M move · q quit · Q quit+stop
```

| Key | Action |
|---|---|
| `↑` `↓` (or `k` `j`), `g` `G` | Select a torrent / jump to first or last |
| `a` | Add a torrent: type or paste the path of a `.torrent` file (quotes, `\ ` escapes and `~` are understood, so dragging a file into the terminal works) or a magnet link |
| `/` | **Search** torrents by text (see below) |
| `p` or space | Pause the selected torrent, or resume it if it is paused or failed |
| `x` or Delete | Remove it (asks first: `y` keeps the files, `d` also deletes them) |
| `i` or Enter | Details of the selected torrent (any key goes back) |
| `s` | Change the download folder (created if missing) |
| `w` | Set the watch folder (`off` turns it off) |
| `m` | Set the folder completed torrents move to (`off` turns it off) |
| `M` | Completed folder of the **selected torrent only**: a folder, `off` (never move it) or `default` (follow the global setting) |
| `q` or Ctrl-C | Quit the view. **Downloads keep running in the background daemon** |
| `Q` | Quit the view and stop the daemon |

Errors (an unreadable path, a duplicate torrent, a daemon problem) are shown in the bottom line and
the prompt stays open so you can fix the text. `NO_COLOR=1` turns colours off. The terminal is
restored on exit, on a crash and on SIGTERM/SIGHUP. When stdin/stdout is not a terminal (pipes,
scripts), `wisplet` with no arguments prints the help instead.

### Subcommands

```sh
# Add downloads (starts the daemon automatically if it is not running)
wisplet add ubuntu.torrent
wisplet add "magnet:?xt=urn:btih:..." -o ~/Downloads/iso
wisplet add big.torrent --move-to /Volumes/DATA/Films   # move this one when it completes

# Search (needs curl) and download a result
wisplet search "debian netinst"            # numbered results, best seeded first
wisplet search debian --source archive --limit 10 --magnet
wisplet search "debian netinst" --add 1    # download result number 1 (starts the daemon if needed)
wisplet sources                           # list the search sources
wisplet list --json                       # machine-readable output (also: info, search, sources, config, status)
wisplet mcp                               # MCP server for LLMs (see "Using it from an LLM")
wisplet guide                             # the guide for LLMs and scripts

# Settings
wisplet config                                  # show the settings
wisplet config set download-dir ~/Torrents      # folder used when `add` has no -o
wisplet config set watch-dir ~/Torrents/inbox   # auto-add .torrent files dropped here (off to disable)
wisplet config set move-dir /Volumes/DATA/Done  # move completed torrents here (off to disable)
wisplet move 3 /Volumes/DATA/Films              # completed folder of one torrent (none | default)

# Inspect
wisplet list          # table with state, progress, speed, ETA, peers (alias: ls)
wisplet watch         # the same, refreshing every second
wisplet info 1        # details of one torrent

# Control
wisplet pause 1       # or: pause all
wisplet resume 1      # also retries torrents that failed
wisplet remove 1      # forget the torrent, keep the files (alias: rm)
wisplet remove 1 --purge   # also delete the downloaded files

# Daemon lifecycle
wisplet start         # start it in the background (accepts the options below)
wisplet status
wisplet stop
wisplet autostart on  # start the daemon at login (macOS); also: off | status

# No daemon: foreground download with a progress bar
wisplet get file.torrent -o .
```

Example `list` output:

```
ID   STATE        PROGRESS          %       SIZE         DOWN       ETA PEERS  NAME
1    downloading  [#####.....]   52.3  756.0 MiB    3.3 MiB/s     1m09s    23  debian-13.7.0-amd64-netinst.iso
2    complete     [##########]  100.0    1.4 GiB            -         -     0  other.iso
3    paused       [##........]   18.0    4.2 GiB            -         -     0  dataset.tar
```

If a download is interrupted (Ctrl-C, reboot, `pause`), resuming it **verifies the data already on
disk** and only downloads what is missing. The daemon remembers its torrents across restarts.

### Watch folder

`wisplet config set watch-dir ~/Torrents/inbox` (or `w` in the live view). From then on, every
`.torrent` file that appears in that folder is added to the daemon automatically and then
**deleted**; the daemon keeps its own copy, so nothing is lost. Details:

- The folder is checked every 5 seconds, and only while the feature is on (no timer runs otherwise).
- A file must be at least 2 seconds old, so a copy or download still in progress is never read
  half written.
- Torrents are added with the normal download folder and the global "completed" folder.
- A torrent that is already in the list is deleted without being added twice.
- A file that is not a valid torrent is renamed to `name.torrent.rejected` instead of being
  retried forever (or deleted: it may be something you want to look at).
- Only regular files ending in `.torrent` (any case) are touched. Other files, folders and
  symlinks are left alone, and files over 64 MiB are rejected.

### Moving completed torrents

`wisplet config set move-dir /Volumes/DATA/Done` (or `m`) moves every torrent that completes to
that folder; `wisplet add FILE --move-to DIR`, `wisplet move <id> <DIR|none|default>` or `M`
set the destination of a single torrent, which wins over the global one (`none` = keep this one
where it was downloaded, `default` = follow the global setting again).

- The file or folder the torrent created is moved as a whole; its name does not change.
- **Nothing is ever overwritten.** If the destination already has an item with that name the
  files stay where they were and the torrent's `info` shows a `note` (`move failed: …`).
- Within one disk it is an instant rename. Across disks (for example to an external drive) the
  data is copied to `name.tdpart`, renamed into place, and only then is the original deleted, so
  an interruption never loses data. While it copies, the state is `moving` and the daemon keeps
  answering.
- The global folder is read when a torrent completes, so changing it affects torrents that are
  still downloading. Setting a destination on an already complete torrent moves it right away.
- A move interrupted by a restart is resumed at the next start. After the move, `info` and
  `remove --purge` use the new location. Removing a torrent while it is moving is refused.
- The daemon does not upload, so moving the files never interferes with seeding.

### Searching for torrents

BitTorrent has no text search of its own (the DHT only finds peers for a torrent you already
identified), so wisplet asks an index. It ships with two sources and **no third-party torrent
site**:

| Source | What it is |
|---|---|
| `archive` | The [Internet Archive](https://archive.org): free and public-domain software, books, audio and films, each with a `.torrent`. Always available. |
| `torznab` | **Your own** indexer manager. Any server that speaks the [Torznab](https://torznab.github.io/spec-1.3-draft/) API (Jackett, Prowlarr, …) — set `WISPLET_TORZNAB_URL` to its address and it is used. Off until configured. Which indexers sit behind it is your decision and your responsibility, exactly as with Sonarr or Radarr. |

```sh
export WISPLET_TORZNAB_URL="http://127.0.0.1:9117/api/v2.0/indexers/all/results/torznab/api?apikey=YOUR_KEY"
wisplet sources          # shows both, and whether torznab is configured
```

Plain `http://` is only accepted for a Torznab server on this machine (`localhost`, `127.0.0.1`,
`::1`); anything else must be `https://`. The API key in the URL is never printed in results.

All configured sources are asked at the same time and one failing never spoils the rest (their
status is printed when any fails, or with `-v`). Results are then **merged by info-hash** (the same
torrent from two sources appears once, labelled `archive+torznab`, with the best seeder count),
magnets are completed with a list of public trackers, torrents **nobody seeds are hidden** (a magnet
with no seeders cannot even fetch its metadata; use `--include-dead` to see them), and the list is
sorted by seeders.

In the live view press `/`, type the text and Enter; a progress line shows while the sources answer
(Esc cancels). Then:

- `↑`/`↓` move through the results; **Enter adds the selected one to your downloads and stays on the
  results**, so you can move to another and press Enter again to add several, one by one.
- A column **IN DOWNLOADS** marks every result that is already there: `✓ downloading`, `✓ complete`,
  `✓ paused`…, or `✓ added` right after you add it. The mark follows the torrent's real state while
  you look at the list, also for torrents you added earlier or from another terminal. Pressing Enter on
  a marked result adds nothing and says so ("Already in your downloads (downloading): …").
- **Esc goes back** to the downloads list, with the newest torrent selected. `/` searches again.

`wisplet search … --add 1,3,4` does the same non-interactively (each result is reported on its own,
so an already-added one does not stop the rest). Results with a magnet are added by magnet; those
that only offer a `.torrent` URL (the Internet Archive) are fetched first.

Things to know:

- **It needs `curl`**, which macOS includes. wisplet has no TLS library (that would triple the
  binary), so every HTTPS request goes through `curl`. The `WISPLET_CURL` environment variable
  selects another executable.
- The text you search for is sent to the sources, like in any browser.
- Availability changes: a source that does not answer simply shows `error: …` in the status and the
  rest still work.

### Adding your own search sources

wisplet publishes the mechanism, not a catalogue: you decide which sources to add, and you are
responsible for what they return. There are two ways, both explained step by step in
**[docs/EXTENDING.md](docs/EXTENDING.md)**:

1. **No code — a Torznab server.** Point `WISPLET_TORZNAB_URL` at your Jackett or Prowlarr and enable
   whatever indexers you want inside it.
2. **A small Rust file.** A source is two functions (build the request, parse the answer) and one
   registry entry, in a file called `src/search_extra.rs` that git ignores. Start from the working,
   tested example:

   ```sh
   cp docs/search_extra.example.rs src/search_extra.rs   # edit the key, the URL and the parser
   ./install.sh --features extra-sources                 # or: cargo build --release --features extra-sources
   wisplet sources                                       # your source is listed
   wisplet search "something" --source yourkey
   ```

   Parsers are pure functions, so you can unit-test them with a canned answer, with no network
   (`cargo test --features extra-sources`). If the file exists but you build without the feature,
   the installer tells you instead of silently leaving your sources out.

### Download speed

What most influences speed is how many peers will actually send you data, and wisplet never
uploads, so well-behaved peers rate you low. To compensate it:

- announces to **HTTPS trackers** through `curl` as well as HTTP/UDP ones, and asks the DHT, so the
  first peers arrive quickly;
- keeps a **request queue per peer that grows with the speed that peer delivers** (10 requests at
  first, up to 64, about 3 seconds of data), so a fast or distant peer is never left waiting;
- **drops peers that keep you choked** after 45 s when other candidates are waiting, to rotate
  through the swarm instead of sitting on slots that never send anything (with nobody waiting it keeps
  them, up to 5 minutes).

`--pipeline N` fixes the queue at N (turning the adaptive behaviour off); `--pipeline-max N` changes
its ceiling; `--max-peers N` changes the connections per torrent. Measurements and what did and did not
help are in [docs/PERFORMANCE.md](docs/PERFORMANCE.md).

### Using it from an LLM (Claude, MCP) or a script

Everything the live view does is also available to a model. Register the MCP server once:

```sh
wisplet mcp-config                     # prints the exact snippets for this machine
claude mcp add wisplet -- /full/path/to/wisplet mcp        # Claude Code
```

(Claude Desktop: add `{"mcpServers":{"wisplet":{"command":"/full/path/to/wisplet","args":["mcp"]}}}`
to `claude_desktop_config.json`.) After that you can talk to it in plain language:

> *"How are my downloads going?"* · *"Add this magnet link: magnet:?xt=…"* ·
> *"Search for ubuntu 24.04 server and download the best one"* · *"Pause everything"* ·
> *"From now on move finished downloads to /Volumes/DATA/Done"*

The server exposes 13 tools: `list_downloads`, `get_download`, `add_download`, `pause_downloads`,
`resume_downloads`, `remove_download`, `search_torrents`, `download_search_results`,
`list_search_sources`, `get_settings`, `update_setting`, `set_move_destination` and `daemon_control`.
It talks to the same daemon as the terminal, starts it when needed, keeps no state of its own, and uses
only stdin/stdout (it opens no network port).

For models that can run commands but have no MCP, and for scripts, these print JSON:
`list --json`, `info ID --json`, `search TEXT --json`, `sources --json`, `config --json`, `status --json`.
`wisplet guide` prints the full guide for the model (tools, fields, workflows, safety rules,
error messages), which is also [docs/LLM.md](docs/LLM.md) and summarised in [llms.txt](llms.txt).

Safety by design: paths from a model must be absolute (relative ones are refused, they would depend on
a directory the model cannot see); `remove_download` keeps the files unless `delete_files` is set;
destructive tools are flagged to the client so it can ask you first; search results are sanitised
(control characters removed, length bounded) and labelled as untrusted data.

### Starting at login (macOS)

`install.sh` runs `wisplet autostart on`, which writes the LaunchAgent
`~/Library/LaunchAgents/com.wisplet.daemon.plist`. From the next login the daemon is running
and resumes your torrents without you opening anything. Details:

- The agent has no `KeepAlive`: a daemon you stop with `wisplet stop` stays stopped until the
  next login, and a crash is not restarted in a loop.
- `autostart on/off` never starts or kills the daemon that is running right now; it takes effect at
  the next login. It records `$WISPLET_HOME` if you set it, and the daemon's output goes to `daemon.log`.
- If you move or reinstall the binary elsewhere, run `wisplet autostart on` again
  (`autostart status` warns when the recorded binary no longer exists).
- It idles at ~1 MiB and 0 % CPU, so leaving it running costs nothing noticeable.

### Download folder

A torrent is saved to, in order of precedence:

1. the `-o/--output DIR` given to `add`;
2. the **download folder setting** (`config set download-dir`, or the `s` key in the live view).
   It is saved in the state directory and survives daemon restarts;
3. the `--download-dir DIR` given to `start`/`daemon`, used only until a setting is saved;
4. `~/Downloads` if it exists, otherwise the current directory.

Changing the setting affects torrents added afterwards; torrents already added keep their folder.

### Options

| Option | Effect | Default |
|---|---|---|
| `-o, --output DIR` | Destination folder of one torrent (`add`/`get`) | the download-folder setting / current directory for `get` |
| `--move-to DIR` | Where to move this torrent when it completes (`none` = never move it) | the global `move-dir` |
| `--download-dir DIR` | Initial download folder of the daemon (a saved setting wins) | `~/Downloads` |
| `--max-peers N` | Peer connections **per torrent** | 30 |
| `--pipeline N` | Fixed number of outstanding 16 KiB requests per peer (turns adaptive off) | adaptive, 10..64 |
| `--pipeline-max N` | Ceiling of the adaptive request queue | 64 |
| `--memory-mb N` | RAM budget for in-flight pieces, **per torrent** | 16 |
| `--no-dht` | Do not use the DHT | DHT on |
| `--peer IP:PORT` | Extra peer to try (repeatable). Useful for testing | — |
| `--source A,B` | `search`: only these sources (`wisplet sources`) | all |
| `--limit N` | `search`: at most N results | 30 |
| `--include-dead` | `search`: also show torrents nobody seeds | hidden |
| `--magnet` | `search`: print each result's magnet link | — |
| `--add N[,N…]` | `search`: download these result numbers | — |
| `--json` | Machine-readable output (`list`, `info`, `search`, `sources`, `config`, `status`) | — |
| `-v, --verbose` | Log to stderr | — |

Engine options (`--max-peers`, `--memory-mb`, …) apply when the **daemon starts**:
`wisplet start --memory-mb 4 --max-peers 15`. If `add` starts the daemon by itself, defaults are used.

### Where things are stored

`$WISPLET_HOME`, or `~/.local/share/wisplet/` if unset:

| File | Contents |
|---|---|
| `session.txt` | The torrent list (id, state, destination, source, completed-folder override) |
| `settings.txt` | Saved settings: download folder, watch folder, completed folder |
| `torrents/<hash>.torrent` | A copy of every `.torrent`, and of metadata fetched through a magnet |
| `search/` | `.torrent` files downloaded for search results and `https://` URLs added by a model |
| `daemon.sock` | Unix control socket (mode `0600`; the directory is `0700`) |
| `daemon.log` | Output of a daemon started with `start` |

## What is and is not supported

**Supported:** BEP 3 (base protocol), BEP 5 (DHT, lookups only), BEP 9 and 10 (metadata from
magnets), BEP 15 (UDP trackers), BEP 23 (compact peers), HTTP and HTTPS trackers (HTTPS through `curl`), IPv4 and IPv6 peers,
single- and multi-file torrents, resume with verification, *endgame* mode, private torrents
(they do not use the DHT, BEP 27).

**Not supported (by design or for now):**

- **It does not upload** (no seeding). That is what was asked for, but it means some peers will
  give you less speed, and it is **not suitable for private trackers that require a ratio**.
- **No built-in TLS.** HTTPS trackers and the search use the system `curl`; without it, HTTPS
  trackers are skipped (the DHT and the other trackers still work) and `search` reports an error.
- Protocol encryption (MSE/PE), uTP, PEX, DHT over IPv6, file selection, speed limits.
- A piece that fails verification is downloaded again, but the offending peer is not identified or banned.
- The daemon and the `add/list/config/...` commands need Unix (macOS/Linux). On Windows only
  `get` exists (untested).

## Responsible use

wisplet is a general-purpose, standards-based BitTorrent client, like Transmission or qBittorrent. It
is meant for content you have the right to download and share: Linux distributions and other
open-source software, public-domain and Creative-Commons works (the built-in Internet Archive source
exists for that), your own files, and anything else whose licence allows it. It does not include or
promote any index of copyrighted material, and downloading material you have no right to is your
responsibility and may be illegal where you live.

## Development

```sh
cargo test                 # the whole suite (339 tests, ~1 min)
cargo clippy --all-targets # no warnings
scripts/bench.sh           # measure CPU and RAM (see docs/PERFORMANCE.md)
```

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — design, modules, flows and decisions.
- [docs/PERFORMANCE.md](docs/PERFORMANCE.md) — what keeps usage low and how it was measured.

Everything in the repository (code, comments, docs, CLI output) is in English.

## Security

- Names and paths inside a `.torrent` are validated: `..`, separators, NUL and duplicates are
  rejected, so a hostile torrent cannot write outside the destination folder.
- An existing file larger than expected is never truncated (the download aborts).
- Magnet metadata is accepted only if its SHA-1 matches the info-hash.
- Every size coming from the network is bounded (messages, bitfield, metadata, bencode depth,
  tracker responses and control-socket requests).
- The control socket is private to the user (`0600`).
- `curl` is only run for `https://` URLs (plain `http://` only for tracker announces done by the
  built-in client), the URL is passed as the value of `--url` so it cannot be read as an option,
  `--proto`/`--proto-redir` forbid `file://` and other schemes even through redirects, `.curlrc` is
  ignored, and the response size and time are bounded. Search results are untrusted data: archive
  identifiers are restricted to safe characters before they become part of a URL.
- The download-folder setting must be an absolute path; the CLI makes relative paths absolute
  before sending them to the daemon.
