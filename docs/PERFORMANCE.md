# Performance: CPU and RAM

The project's goal is the **lowest possible CPU and RAM use**. This document explains which
decisions achieve it, what was measured and how, and what those measurements do **not** prove.

## What keeps it light

| Decision | Effect |
|---|---|
| One thread (`current_thread` + `LocalSet`) | No thread pool, no locks, no cross-thread context switches. |
| Per-torrent memory budget (`--memory-mb`) | In-flight piece RAM has a hard ceiling: `memory_budget` (at least one piece). |
| Messages borrow the read buffer | A 16 KiB block is parsed without allocating and copied exactly once. |
| Piece buffers recycled in a pool | No allocator churn during a long download. |
| Positional writes (`pwrite`) with a single open file | No intermediate buffers and no file descriptors growing with the file count. |
| Hardware-accelerated per-piece SHA-1 (`sha1` crate) | Verification costs a small fraction of the CPU. |
| No polling | Tasks sleep on `Notify`/sockets/timers; the idle daemon does zero work. |
| Everything bounded | Peer queue (4096), DHT candidates (64), messages, metadata (8 MiB), bencode (depth 64)… |
| Minimal dependencies + `lto="fat"`, `panic="abort"`, `strip` | ~1.2 MiB binary (it was 0.9 MiB before the live view, search, MCP server and JSON were added) and instant start-up. |
| No TLS, no HTTP/DNS crates, no serde | Less code, less base memory. |

The settings, the live view, the login item and the move-on-completion add no background work to the daemon, and the watch folder only runs a 5-second scan while it is enabled (off by default; a scan is one `read_dir`): the settings are read once at start and written only when changed. The live view is a separate client process that exists only while you have it open (about 1.5 MiB and no measurable CPU when idle: one daemon request and one redraw every 500 ms), and the login item is just a file that starts the same daemon.

## Results

Machine: Apple M4, macOS (Darwin 25.6), Rust 1.97, `release` profile. One run per scenario.

### Memory: footprint vs RSS

`ps`/`top` show the **RSS**, which on macOS includes ~8 MiB of shared, read-only pages from system
libraries (not memory owned by the process). The figure that best represents what the program
costs is the **physical footprint** (`vmmap -summary <pid>`, *Physical footprint*). In the real
download with defaults: footprint 19.7 MiB, of which 17.8 MiB are piece buffers, against 27 MiB RSS.

| Scenario | Footprint | RSS |
|---|---|---|
| Idle daemon, no torrents | 1.1 MiB | 2.0 MiB |
| Idle daemon, 1 finished torrent | 1.6 MiB | 2.6 MiB |
| Real download, defaults (16 MiB budget, 30 peers) | **19.7 MiB** | 27 MiB |
| Real download, `--memory-mb 4 --max-peers 15` | **7.5 MiB** | 15 MiB |

RSS in the real download stayed **flat at 27 MiB** over the 70 s sampled (no leaks).

### CPU in a real download

Debian 13.7.0 netinst (756 MiB, public torrent) with `get` and defaults:

| Metric | Value |
|---|---|
| Duration | 3 min 46 s (3.3 MiB/s average, limited by the swarm) |
| Total CPU | 6.64 s (3.21 user + 3.43 system) = **2.9 %** of one core on average |
| Integrity | File SHA-256 identical to the one published by Debian |

The same ISO downloaded from a **tracker-less magnet** (DHT + peer metadata only) also completed correctly.

With `--memory-mb 4 --max-peers 15`: 0.72 s of CPU in the first minute. This is **not directly
comparable** with the row above because the speed reached in that minute was not recorded.

### Loopback benchmark (reproducible)

`scripts/bench.sh [MiB] [piece KiB] [number of seeders]` downloads from synthetic seeders running
in **another process** (so their cost is not added). Loopback removes network latency, so the
client never waits: this is the **worst case of CPU per byte**.

| Scenario | Speed | CPU | CPU/GiB | Peak RSS |
|---|---|---|---|---|
| 256 MiB, 1 seeder, 256 KiB pieces | 191 MiB/s | 0.80 s | 3.2 s | 2.8 MiB |
| 512 MiB, 8 seeders, 4 MiB pieces, 16 MiB budget | 170 MiB/s | 2.08 s | 4.2 s | 10.4 MiB |
| 512 MiB, 8 seeders, 4 MiB pieces, 4 MiB budget | 166 MiB/s | 2.10 s | 4.2 s | 6.4 MiB |
| 128 MiB, 3 seeders, 256 KiB pieces | 113 MiB/s | 0.58 s | 4.6 s | 3.8 MiB |

Lowering the budget from 16 to 4 MiB barely changed the speed in this scenario.

### Comparison with Transmission (same torrent, same time)

Ubuntu Server 26.10 (2.7 GiB, 256 KiB pieces) added to Transmission (the macOS app, idle apart from
this torrent) and to `wisplet get` **at the same moment**, so both saw the same swarm. Both files
have the same SHA-256. CPU is measured per process until each one finished.

| | wisplet | Transmission |
|---|---|---|
| Time to complete | 988 s | 397 s |
| CPU used | 21 s | 36 s |
| CPU per GiB downloaded | **7.8 s** | 13.3 s |
| Average CPU load | 2.1 % of a core | 9.1 % of a core |
| Physical footprint | **~19–20 MiB** | ~113 MiB average (94 MiB before the download) |
| Peak RSS | 28 MiB | 125 MiB |

How to read it:

- **Memory and CPU per byte favour wisplet** (about 6x less memory, about 40 % less CPU per GiB).
  This is expected: Transmission is a full GUI application that also uploads, keeps a larger
  connection set and runs a UI. Part of the gap is functionality, not inefficiency.
- **Speed favoured Transmission by 2.5x.** wisplet had 0 peers for the first ~90 s because this
  torrent only lists HTTPS trackers (unsupported) and it had to rely on the DHT. After that it was
  slow and irregular (0.8-1.5 MiB/s, later 3-4 MiB/s). The causes were not diagnosed; plausible ones
  are no upload (peers unchoke non-reciprocating peers less), 30 TCP-only connections versus
  Transmission's larger set plus uTP/PEX, and sharing the same line. Treat wisplet as slower in
  swarms like this one until proven otherwise.
- One run each, one machine, one torrent, both competing for the same bandwidth. Transmission's CPU
  includes its uploads and its UI.

### Daemon with watch folder and move-on-completion (Ubuntu again)

The same Ubuntu Server 26.10 torrent, this time through the **daemon** with both new features on:
`watch-dir` and `move-dir` set, the `.torrent` dropped into the watch folder, nothing else typed.
The daemon added it, deleted the dropped file, downloaded it and moved the ISO to the completed
folder (a rename on the same disk). The SHA-256 of the moved file matches the one from the earlier
runs. Sampled every 5 s from the daemon's own process.

| | Value |
|---|---|
| Time from drop to ISO in the completed folder | 773 s (swarm-dependent; 988 s in the earlier `get` run, Transmission 397 s) |
| CPU used by the daemon | 25 s = **9.3 s per GiB**, 3.2 % of a core on average |
| Peak physical footprint / RSS | **21.0 MiB** / 29 MiB |
| Idle, watch folder on, no torrents (30 s) | 0.00 s CPU, 1.2 MiB |
| Idle, watch folder on, one finished torrent (30 s) | 0.00 s CPU, **21.0 MiB** (see below) |

Reading it:

- The watch folder costs nothing measurable: 30 s with it enabled used no CPU (one `read_dir`
  every 5 s), and moving on the same disk is instantaneous.
- CPU per GiB (9.3 s) is a little higher than the 7.8 s of the earlier `get` run and still below
  Transmission's 13.3 s in the simultaneous run. Both are single runs with different swarm
  conditions and the sampler adds its own load, so do not read the difference as meaningful.
- **Finding:** after the download finished the daemon's footprint stayed at ~21 MiB instead of
  returning to ~1 MiB, even though the piece buffers are freed. A freshly restarted daemon with the
  same finished torrent measured 1.6 MiB earlier, so the memory is held by the process (most likely the
  allocator keeping freed pages), not needed by the torrent. The cause was not confirmed. Restarting
  the daemon releases it; handing memory back to the OS after a download is a possible improvement.
- **Transmission was not measured again.** It was closed, and the ISO from the previous test was
  still in its completed folder, which would have made a re-download invalid. The Transmission
  figures in the previous section are from the earlier simultaneous run.

## Download speed

Speed is a different problem from CPU and RAM: it depends on how many peers will actually send data,
and wisplet never uploads, so well-behaved peers rate it low. Real swarms also change from one
minute to the next (the same torrent gave 0.3 MiB/s and 20 MiB/s on the same machine within the same
hour), so **speed can only be compared fairly when the clients run at the same time on the same
swarm**. Everything below that compares versions does that; the single-client runs are listed only
to show how large the variation is.

### What limited it (diagnosis)

1. **Slow start.** The Ubuntu torrent lists only HTTPS trackers, which used to be ignored, so the
   first peers came from the DHT alone: more than 10 s in 4 of 14 runs of the original (40, 40, 40
   and 100 s).
2. **Slots wasted on peers that never send.** A peer that chokes us held its connection slot up to
   5 minutes, and with all candidates already tried nobody replaced it.
3. **A fixed queue of 10 requests per peer.** That caps one connection at roughly
   `10 x 16 KiB / round-trip time`: ~1.6 MiB/s at 100 ms. A fast or distant peer cannot be used fully.

### What changed

| Change | Why |
|---|---|
| HTTPS trackers through `curl` | peers from the very first second on torrents like Ubuntu's |
| Drop a peer that keeps us choked for 45 s *when others are waiting* | rotate through the swarm instead of sitting on dead slots |
| Discovery asks again when the candidate list runs low (min. 60 s between rounds) | so there is always someone to rotate to; `numwant` 200 |
| Request queue per peer grows with its measured speed (10 to 64, about 3 s of data) | use fast and distant peers fully |
| **Hoarding guard**: a peer holding more than the minimum that sends nothing for 5 s has the extra requests cancelled and returned | found by measurement, see below |

### What the measurements showed

Time to the first peers (job of the trackers): **0 of 22 runs of the new version took more than
10 s; 4 of 14 runs of the original did.** This is the clearest, most consistent gain.

Throughput, MiB/s over 120 s, three versions downloading at once from the same swarm (4 rounds each,
start order rotated):

| | R1 | R2 | R3 | R4 | mean |
|---|---|---|---|---|---|
| original | 1.64 | 2.05 | 1.52 | 1.59 | 1.70 |
| new, queue fixed at 10 (`--pipeline 10`) | 1.28 | 1.90 | 1.04 | 1.53 | 1.44 |
| new, adaptive queue, **no** hoarding guard | 1.57 | **0.05** | 6.00 | 1.74 | 2.34 |

The adaptive queue was the only variant that ever beat the others clearly (6.00 vs ~1.5 in one
round), but it also produced one near-stall (0.05 MiB/s): a peer that had a fast burst kept dozens of
requests, went silent, and its blocks sat for the 30-60 s request timeout in the pieces that the
memory budget allows in flight, starving everyone else. That is what the guard fixes (its test fails
without it). Repeating the experiment with the guard:

| | R1 | R2 | R3 | R4 | mean |
|---|---|---|---|---|---|
| original | 0.01 (peers took 100 s) | 2.19 | 1.80 | 1.53 | 1.38 |
| new, queue fixed at 10 | 1.52 | 1.57 | 0.92 | 1.54 | 1.39 |
| new, adaptive + guard | **3.20** | 1.62 | **3.12** | 1.92 | **2.47** |

With the guard no round collapsed and adaptive was ahead of the original in 3 of 4 rounds. The
other changes alone (the fixed-queue row) are no faster than the original once the peers arrive.
**This is 4 + 4 rounds on one network: an indication, not a proof.**

What did **not** help: more connections (`--max-peers 60`: 2.85 and 2.33 vs 3.23 and 0.25 for 30 in
the first, unpaired batch, i.e. within the noise), and trying to hand freed memory back to the
operating system after a download (see below).

### Against Transmission, same swarm, same moment

Ubuntu Server 26.04.1 (2.73 GiB), Transmission and two wisplet builds started together on the
same torrent (SHA-256 of the three files identical). CPU is the process's total until it finished.

| | Time | Average speed | CPU | CPU per GiB | Footprint (peak) | RSS (peak) |
|---|---|---|---|---|---|---|
| **wisplet (this version)** | **160 s** | **17.5 MiB/s** | 14 s | **5.1 s** | **20 MiB** | 28 MiB |
| wisplet (original) | 295 s | 9.5 MiB/s | 15 s | 5.5 s | 20 MiB | 27 MiB |
| Transmission | 223 s | 12.5 MiB/s | 54 s | 19.8 s | 91 MiB | 153 MiB |

In this run wisplet was 1.4 times faster than Transmission and 1.8 times faster than its own
previous version, while using about 4 times less CPU per GiB and 4.5 times less memory. Caveats that
matter: it is **one run** on one network and the swarm happened to be fast (the
earlier run of the original against Transmission, in a slow period, went the other way: 988 s against
397 s); Transmission has a GUI and also uploads, and its CPU includes both. The earlier comparison in the
previous section was taken with the original version and is kept for reference.

### Memory held after a download

After a download finishes the process keeps a footprint close to its peak (about 20 MiB after a real
swarm download) instead of returning to ~1 MiB; a restarted daemon is back at 1-2 MiB. Calling the
allocator's release function (`malloc_zone_pressure_relief`) after each torrent was tried and made no
measurable difference on macOS, so it was removed. The footprint is still bounded by the memory
budget; restarting the daemon releases it. The adaptive queue slightly raises the peak (more
pieces in flight) in exchange for speed: 6.6 MiB against 3.2 MiB after a 512 MiB loopback download.

## What these measurements do NOT show

- **The only comparison with another client is the single Transmission run above** (nothing for librqbit, qBittorrent…). The claim
  that "from scratch is more efficient than librqbit for download-only" is a design argument
  (fewer features and dependencies), **not a measured result**. To check it, both would have to
  run the same torrent and be measured with the same method.
- The real-world numbers are **a single run** on **one machine** and depend on the state of the
  swarm at the time. Treat them as an order of magnitude.
- The idle figures of the live view were measured once (30 s, one process); the download
  figures were taken before it existed and are unaffected, since it is a separate client.
- The loopback benchmark includes no latency, packet loss or slow peers.
- The effect of lowering `--memory-mb` on **speed** in real swarms was not measured: fewer
  in-flight pieces may limit parallelism when pieces are very large.
- It has not been tested on Linux or Windows.
- Speed conclusions rest on a handful of rounds on one connection; run `scripts/bench.sh` or the same-time method above on your own network before relying on them.

## Tuning

| I want… | Do this |
|---|---|
| Minimum RAM | `--memory-mb 4` (or less). With very large pieces the effective minimum is one piece. |
| Less CPU/connections | `--max-peers 10..15` |
| More speed on high-latency networks | `--pipeline 16..32` (more outstanding requests per peer) |
| Less discovery traffic | `--no-dht` (only if the torrent has working UDP/HTTP trackers) |

The limits are **per torrent**: with several torrents active at once, consumption adds up.

## Known costs and possible improvements

- **Resuming re-hashes** data already on disk (no bitfield is stored). It is a one-off CPU cost,
  not a sustained one, but on torrents of tens of GiB it is noticeable at start-up.
- About a third of the CPU in loopback is system time (16 KiB socket reads). A bigger read buffer
  would cut system calls at the price of ~48 KiB more per connection; this was not done because on
  real networks the bottleneck is the network.
- Piece buffers are zero-filled when allocated (`resize`); a lazy `calloc` would avoid touching
  pages that are not used yet.

## Reproducing

```sh
scripts/bench.sh 256 256 1                      # loopback, 1 seeder
CLIENT_FLAGS="--memory-mb 4" scripts/bench.sh 512 4096 8
# physical footprint of a running process (macOS):
vmmap -summary <pid> | grep "Physical footprint"
# accumulated CPU and RSS:
ps -o cputime=,rss= -p <pid>
```

`scripts/bench.sh` aborts with an error if the torrent does not complete, so it never measures nothing.
