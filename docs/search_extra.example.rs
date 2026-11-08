//! EXAMPLE of `src/search_extra.rs`: how to add your own search source.
//!
//! Copy this file to `src/search_extra.rs` (which git ignores), edit it, and build with
//! `--features extra-sources`. See `docs/EXTENDING.md`. This example talks to an imaginary open
//! catalogue (`catalog.example.org`) that answers JSON like:
//!
//! ```json
//! {"items":[{"title":"Some Linux ISO","infohash":"2c6b6858d61da9543d4231a71db4b1c9264b0685",
//!            "size":3654957056,"seeders":34,"leechers":11}]}
//! ```
//!
//! This file is compiled and tested together with the rest of the crate, so it cannot go stale.

use super::*; // the helpers of the search module: normalize_hash, make_magnet, field_str, ...

/// The one function the crate calls: every source you want to register.
pub fn sources() -> Vec<Source> {
    static DEFS: [SourceDef; 1] = [SourceDef {
        key: "catalog", // what you type after `--source` and what `wisplet sources` lists
        description: "An example open catalogue (JSON API)",
        request: catalog_request,
        parse: parse_catalog,
    }];
    DEFS.iter().map(Source::from_def).collect()
}

/// Build the request. Return `None` when the source is not usable (for example a missing
/// setting); it is then simply left out of the search. Only `https://` is fetched (`curl` does it).
fn catalog_request(query: &str, _torznab_url: Option<&str>) -> Option<SourceRequest> {
    Some(SourceRequest {
        url: format!("https://catalog.example.org/api/search?q={}", percent_encode(query.as_bytes())),
        body: None,        // Some(json_bytes) makes it a POST with a JSON body
        allow_http: false, // keep false unless the server is on this very machine
    })
}

/// Turn the answer into results. Pure (text in, results out) so it can be tested with a canned
/// answer; return `Err` for an answer that is not what you expect and the search reports it.
fn parse_catalog(text: &str) -> Result<Vec<SearchResult>> {
    let json = Json::parse(text).map_err(bad_json)?;
    let items = json.get("items").and_then(Json::as_array).ok_or_else(|| Error::Failed("unexpected answer".into()))?;
    let mut out = Vec::new();
    for item in items {
        // skip what cannot be used instead of failing the whole answer
        let (Some(hash), name) = (normalize_hash(field_str(item, "infohash")), field_str(item, "title")) else { continue };
        if name.is_empty() {
            continue;
        }
        out.push(SearchResult {
            source: "catalog".into(), // shown in the SOURCE column
            name: name.into(),
            size: field_u64(item, "size"),
            seeders: clamp32(field_u64(item, "seeders")),
            leechers: clamp32(field_u64(item, "leechers")),
            magnet: make_magnet(&hash, name), // or the source's own magnet link
            url: String::new(),               // a .torrent URL, if the source has no hashes
            info_hash: hash,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_source_is_registered_and_builds_a_safe_request() {
        let src = sources()[0];
        assert_eq!(src.key(), "catalog");
        let req = src.request("debian 13 & more", None).unwrap();
        assert_eq!(req.url, "https://catalog.example.org/api/search?q=debian%2013%20%26%20more");
        assert!(req.body.is_none() && !req.allow_http);
    }

    #[test]
    fn the_example_parser_reads_a_canned_answer_and_skips_unusable_items() {
        let answer = r#"{"items":[
            {"title":"Some Linux ISO","infohash":"2C6B6858D61DA9543D4231A71DB4B1C9264B0685","size":3654957056,"seeders":34,"leechers":11},
            {"title":"no hash","seeders":5},
            {"title":"","infohash":"232cd67eb3ffbd7c37bf9ec3ee887417e5ae1ee6"}]}"#;
        let found = sources()[0].parse(answer.as_bytes()).unwrap();
        assert_eq!(found.len(), 1);
        let r = &found[0];
        assert_eq!((r.name.as_str(), r.size, r.seeders, r.leechers), ("Some Linux ISO", 3_654_957_056, 34, 11));
        assert_eq!(r.info_hash, "2c6b6858d61da9543d4231a71db4b1c9264b0685");
        assert!(r.magnet.starts_with("magnet:?xt=urn:btih:2c6b6858") && r.magnet.contains("&tr="));
        assert!(sources()[0].parse(b"{}").is_err(), "an unexpected answer is an error, reported per source");
    }
}
