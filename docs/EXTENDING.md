# Adding your own search sources

wisplet ships two search sources: the **Internet Archive** and a **Torznab** connector. Anything else
is up to you, and there are two ways to add it.

## 1. No code: a Torznab server (recommended)

If the thing you want to search is supported by [Jackett](https://github.com/Jackett/Jackett) or
[Prowlarr](https://github.com/Prowlarr/Prowlarr) (or any server that speaks the
[Torznab](https://torznab.github.io/spec-1.3-draft/) API), configure that server once and wisplet
searches through it:

```sh
export WISPLET_TORZNAB_URL="http://127.0.0.1:9696/1/api?apikey=YOUR_KEY"   # Prowlarr / Jackett endpoint
wisplet sources                    # torznab is now listed as available
wisplet search "debian netinst" --source torznab
```

Which indexers you enable inside Jackett or Prowlarr is your decision and your responsibility. Plain
`http://` is accepted only for a server on this machine; anything else must be `https://`. The API
key in the URL is never printed.

## 2. A source in Rust (your own, uncommitted file)

A source is two small functions and a registry entry. The file lives at `src/search_extra.rs`, which
`.gitignore` excludes, and is compiled only with the `extra-sources` feature, so the public project
never contains it:

```sh
cp docs/search_extra.example.rs src/search_extra.rs     # start from the working example
$EDITOR src/search_extra.rs                             # change the key, URL and parser
./install.sh --features extra-sources                   # or: cargo build --release --features extra-sources
wisplet sources                                         # your source is listed
wisplet search "something" --source yourkey
```

The installer prints a note if `src/search_extra.rs` exists but you built without the feature, so a
plain `./install.sh` cannot drop your sources unnoticed.

[`docs/search_extra.example.rs`](search_extra.example.rs) is a complete, commented example; it is
compiled and tested with the crate, so it always matches the real API. In short:

```rust
use super::*;

pub fn sources() -> Vec<Source> {
    static DEFS: [SourceDef; 1] = [SourceDef {
        key: "catalog",                       // --source catalog
        description: "An example open catalogue (JSON API)",
        request: catalog_request,             // fn(query, torznab_url) -> Option<SourceRequest>
        parse: parse_catalog,                 // fn(&str) -> Result<Vec<SearchResult>>
    }];
    DEFS.iter().map(Source::from_def).collect()
}
```

### The contract

| Piece | Rules |
|---|---|
| `request(query, torznab_url)` | Return the URL (and an optional POST body) to fetch, or `None` to leave the source out (for instance when a setting is missing). Percent-encode the query with `percent_encode`. Only `https://` is fetched, by the system `curl`; set `allow_http` only for a server on the same machine. |
| `parse(text)` | **Pure**: text in, `Vec<SearchResult>` out, no I/O. Skip items you cannot use instead of failing the whole answer; return `Err` only when the answer is not what you expected (it is then reported for this source and the others still work). |
| `SearchResult` | `info_hash` (40 lowercase hex, use `normalize_hash`), `magnet` (use `make_magnet` or the source's own), `url` for a `.torrent` when there is no hash, `size` in bytes, `seeders`, `leechers`, a short `source` label. Results from different sources with the same `info_hash` are merged. |
| Helpers | `normalize_hash`, `make_magnet`, `parse_size`, `field_str` / `field_u64` (JSON), `tag_text` / `xml_unescape` (XML), `clamp32`. They are private to the search module and available to your file through `use super::*;`. |

### Good practice

- **Test the parser with a canned answer** (see the two tests at the bottom of the example): no
  network, no flakiness. Run it with `cargo test --features extra-sources`.
- Keep secrets (API keys) out of what you return in `url`; if a link carries one, wisplet shows it
  without its query string, and the full link stays inside the process.
- Be considerate with the servers you query (one request per search, no scraping loops).
- Prefer a documented JSON or RSS API over scraping HTML pages, which break when the layout changes.
- You are responsible for what the sources you add return and for what you download with them.
