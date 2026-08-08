//! The minimap server, as a library: paths in, an axum [`Router`] out.
//!
//! Serves one PMTiles archive per layer over plain `/tiles/{layer}/{z}/{x}/{y}`,
//! the two-file viewer that draws them, and -- when an anon index is given --
//! the `/zone` lookup (see `anon/README.md`). The `minimap-backend` binary is
//! this crate plus an environment and a listen address; another application
//! embeds the same thing by nesting the router under a prefix of its own:
//!
//! ```ignore
//! let map = minimap_server::MapServer::open(&opts)?.router();
//! app = app.nest("/map", map);
//! ```
//!
//! There is deliberately no database, no geometry code and no protobuf library
//! here. Every tile was clipped, simplified, encoded and gzipped by the bake and
//! export steps, so a request is: Hilbert id, two binary searches, and one copy
//! of a few kB out of an mmapped file. Nothing is decompressed -- the stored
//! gzip goes to the client untouched via Content-Encoding.
//!
//! (The copy exists because the response outlives the borrow of the archive. At
//! ~200 ns for an average 6.8 kB tile it is irrelevant next to the ~100 us a
//! cold page fault costs, so it is not worth an owned-slice dance to avoid.)
//!
//! The viewer's own requests are all *relative* -- `meta.json`, `tiles/...`,
//! `zone` -- and the shell is redirected to a trailing-slash URL first, so the
//! same two files work at `/` and under any nest prefix without knowing which.
//!
//! Two constraints the host application has to respect, both explained at
//! length in `anon/README.md` and `server/README.md`: do not log the `/zone`
//! request line (a TraceLayer on the outer router sees nested routes too), and
//! do not re-encode the tile responses (they ship pre-gzipped; a compression
//! layer must exempt them).

pub mod pmtiles;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    extract::{OriginalUri, Path as UrlPath, RawQuery, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};

use pmtiles::Archive;

/// Where everything is. Explicit paths, no environment and no
/// `CARGO_MANIFEST_DIR`: the same rule the pipeline follows with its `Config`,
/// because a library that guesses paths is a library that guesses wrong inside
/// someone else's deployment. The binary is where defaults live.
pub struct Options {
    /// Directory of `<layer>.pmtiles` archives -- `make all`'s deliverable.
    pub tiles: PathBuf,
    /// Directory holding the viewer (`index.html`, `minimap.js`).
    pub web: PathBuf,
    /// The anon zone index (`make anon`). `None`, or a missing file, serves
    /// the map without `/zone`; a file that exists but does not parse is an
    /// error, because a corrupt index should stop a deploy, not ship quietly.
    pub zones: Option<PathBuf>,
    /// Which baked tier `/zone` answers from; `None` means the most private
    /// one in the index. Resolved once here -- never per request, see
    /// `Index::tier` for why a caller-chosen k is no choice at all.
    pub k: Option<u32>,
}

/// One layer's archive, and what a client needs to know about it.
struct Layer {
    name: String,
    archive: Archive,
    /// The zoom rungs this layer holds, verbatim from its archive metadata.
    rungs: String,
    /// Content is immutable within a build, so one etag per archive lets a
    /// browser skip re-downloading tiles it already has -- and, because it is
    /// per archive, a change to one layer does not invalidate the others.
    etag: String,
    bytes: u64,
}

/// Everything `/zone` needs, resolved once at startup.
struct Anon {
    map: memmap2::Mmap,
    index: anon_format::Index,
    tier: usize,
}

/// An open server: the archives, the viewer, and maybe the zones. [`open`] it,
/// then either take [`router`] or read [`report`] first.
///
/// [`open`]: MapServer::open
/// [`router`]: MapServer::router
/// [`report`]: MapServer::report
pub struct MapServer {
    /// In draw order, which is also the order the viewer is told about them.
    layers: Vec<Layer>,
    web: PathBuf,
    anon: Option<Anon>,
}

/// Draw order. The viewer has its own copy for styling, but the server decides
/// what order it hears about them in, so an archive dropped in by hand still
/// lands in the right place.
const ORDER: [&str; 6] = ["land", "landuse", "water", "roads", "buildings", "places"];

type S = Arc<MapServer>;

impl MapServer {
    /// Open every archive in `opts.tiles`, and the zone index if there is one.
    ///
    /// Which layers a build produced is a fact about the build, not a list to
    /// keep in step by hand: a layer with no tiles gets no archive, and this
    /// simply does not find one. No archives at all is an error, since a map
    /// server with no map is a misconfiguration, not a state to serve from.
    pub fn open(opts: &Options) -> Result<MapServer, Box<dyn std::error::Error>> {
        let mut layers: Vec<Layer> = Vec::new();
        for entry in std::fs::read_dir(&opts.tiles)
            .map_err(|e| format!("{}: {e} -- run `make all` first", opts.tiles.display()))?
            .flatten()
        {
            let file = entry.file_name().to_string_lossy().into_owned();
            let Some(name) = file.strip_suffix(".pmtiles") else {
                continue;
            };
            let path = entry.path();
            let archive = Archive::open(&path)?;
            let meta = entry.metadata()?;
            // A build fingerprint: size plus mtime, enough to change whenever a
            // new archive is shipped.
            let etag = format!(
                "\"{:x}-{:x}\"",
                meta.len(),
                meta.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs()
            );
            let rungs = json_field(&archive.metadata, "\"rungs\":").unwrap_or_else(|| {
                format!(
                    "[{}]",
                    (archive.min_zoom..=archive.max_zoom)
                        .map(|z| z.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                )
            });
            layers.push(Layer {
                name: name.to_string(),
                archive,
                rungs,
                etag,
                bytes: meta.len(),
            });
        }
        if layers.is_empty() {
            return Err(format!(
                "no .pmtiles in {} -- run `make all` first",
                opts.tiles.display()
            )
            .into());
        }
        layers.sort_by_key(|l| ORDER.iter().position(|o| *o == l.name).unwrap_or(usize::MAX));

        // A missing index is normal -- the map predates `make anon` -- but a
        // corrupt one is refused loudly rather than served wrongly.
        let anon = match &opts.zones {
            None => None,
            Some(path) => match std::fs::File::open(path) {
                Err(_) => None,
                Ok(file) => {
                    // SAFETY: the index is immutable for the life of the
                    // process; a re-bake writes a new file and the server
                    // restarts onto it, the same contract as the archives.
                    let map = unsafe { memmap2::Mmap::map(&file)? };
                    let index = anon_format::Index::parse(&map)
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    let tier = index.tier(opts.k).ok_or_else(|| {
                        format!(
                            "no tier for k={:?} in {}; baked tiers are {:?}",
                            opts.k,
                            path.display(),
                            index.tiers.iter().map(|t| t.k).collect::<Vec<_>>()
                        )
                    })?;
                    Some(Anon { map, index, tier })
                }
            },
        };

        Ok(MapServer {
            layers,
            web: opts.web.clone(),
            anon,
        })
    }

    /// What got opened, one line per layer -- the report the standalone binary
    /// prints at boot. A string rather than println!, because whether a host
    /// application wants this on its stdout is its call, not this crate's.
    pub fn report(&self) -> String {
        let mut out = String::new();
        for l in &self.layers {
            out.push_str(&format!(
                "  {:<12} {:>9} tiles  z{}..{}  {:>8.1} MB\n",
                l.name,
                l.archive.tile_count,
                l.archive.min_zoom,
                l.archive.max_zoom,
                l.bytes as f64 / 1e6
            ));
        }
        match &self.anon {
            Some(a) => {
                let t = a.index.tiers[a.tier];
                out.push_str(&format!(
                    "  {:<12} {:>9} zones  k={:<10} {:>8.1} MB\n",
                    "anon",
                    t.zones,
                    t.k,
                    a.map.len() as f64 / 1e6
                ));
            }
            None => out.push_str("  anon: no index -- `make anon` enables /zone\n"),
        }
        out
    }

    /// The server as a router, ready to serve at `/` or to be nested under a
    /// prefix. State is applied here, so the result composes with a host
    /// application's own router without touching its state type.
    pub fn router(self) -> Router {
        Router::new()
            .route("/", get(index))
            .route("/meta.json", get(meta_json))
            .route("/tiles/{layer}/{z}/{x}/{y}", get(tile))
            .route("/zone", get(zone_from_query).post(zone_from_body))
            .route("/{*path}", get(asset))
            .with_state(Arc::new(self))
    }
}

/// The viewer shell -- after making sure its URL ends in a slash.
///
/// The redirect is what lets the viewer's requests be relative: `meta.json`
/// resolves against the *document* URL, so at `/map` it would miss the prefix
/// and at `/map/` it lands. Nesting hands this handler both spellings, and
/// only the original URI can tell them apart. Fragments (`#zoom/lat/lon`)
/// survive a redirect in every browser, and the one query parameter is
/// carried by hand.
async fn index(State(s): State<S>, OriginalUri(uri): OriginalUri) -> Response {
    let path = uri.path();
    if !path.ends_with('/') {
        let to = match uri.query() {
            Some(q) => format!("{path}/?{q}"),
            None => format!("{path}/"),
        };
        return Redirect::permanent(&to).into_response();
    }
    serve_file(&s.web.join("index.html"), "text/html; charset=utf-8").await
}

/// Static assets from web/. No directory listing, and any path that could climb
/// out of web/ is refused rather than normalised.
async fn asset(State(s): State<S>, UrlPath(path): UrlPath<String>) -> Response {
    if path.contains("..") || path.contains('\\') || path.starts_with('/') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let content_type = match path.rsplit_once('.').map(|(_, e)| e) {
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    serve_file(&s.web.join(&path), content_type).await
}

async fn serve_file(path: &Path, content_type: &'static str) -> Response {
    match tokio::fs::read(path).await {
        Ok(body) => ([(header::CONTENT_TYPE, content_type)], body).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// One raw JSON value out of the archive's metadata blob.
///
/// A three-line scan rather than a serde dependency: this reads two numbers and
/// one array out of a string the export step wrote, and the backend's whole
/// argument is that it carries no parsing machinery it does not need.
fn json_field(meta: &str, key: &str) -> Option<String> {
    let rest = meta.split_once(key)?.1.trim_start();
    let end = match rest.as_bytes().first()? {
        b'[' => rest.find(']')? + 1,
        _ => rest.find([',', '}'])?,
    };
    Some(rest[..end].trim().to_string())
}

/// TileJSON, read straight out of the archives so the viewer never hardcodes a
/// zoom range, an extent, or which layers exist.
///
/// The per-layer `rungs` are the important part: each layer holds a different set
/// of zoom levels, and the viewer picks, per layer, the deepest rung at or below
/// the zoom being displayed. That is what replaced a single rung list plus a
/// special case for the two background layers.
async fn meta_json(State(s): State<S>) -> Response {
    let attribution = json_string(&s.layers[0].archive.metadata, "\"attribution\":")
        .unwrap_or_else(|| "© OpenStreetMap contributors".into());

    // The map's own range and extent are the union of its layers'.
    let (mut lo, mut hi) = (u8::MAX, 0u8);
    let (mut w, mut sth, mut e, mut n) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for l in &s.layers {
        lo = lo.min(l.archive.min_zoom);
        hi = hi.max(l.archive.max_zoom);
        w = w.min(l.archive.min_lon);
        sth = sth.min(l.archive.min_lat);
        e = e.max(l.archive.max_lon);
        n = n.max(l.archive.max_lat);
    }
    let layers = s
        .layers
        .iter()
        .map(|l| {
            format!(
                r#"{{"name":"{}","rungs":{},"minzoom":{},"maxzoom":{}}}"#,
                l.name, l.rungs, l.archive.min_zoom, l.archive.max_zoom
            )
        })
        .collect::<Vec<_>>()
        .join(",\n  ");

    let body = format!(
        r#"{{"tilejson":"3.0.0","scheme":"xyz","tiles":["tiles/{{layer}}/{{z}}/{{x}}/{{y}}"],
"minzoom":{lo},"maxzoom":{hi},"bounds":[{w},{sth},{e},{n}],"center":[{cx},{cy},{lo}],{anon}
"layers":[
  {layers}
],
"attribution":"{attribution}"}}"#,
        cx = (w + e) / 2.0,
        cy = (sth + n) / 2.0,
        // Present only when /zone will answer, so the viewer can offer the
        // click without probing for the endpoint.
        anon = s
            .anon
            .as_ref()
            .map(|a| format!("\n\"anon\":{{\"k\":{}}},", a.index.tiers[a.tier].k))
            .unwrap_or_default(),
    );
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn tile(
    State(s): State<S>,
    UrlPath((layer, z, x, y)): UrlPath<(String, u8, u32, u32)>,
    headers: HeaderMap,
) -> Response {
    // An unknown layer is a 404, not a 204: "this build has no such layer" and
    // "that tile happens to be empty" are different answers, and only the first
    // means stop asking.
    let Some(l) = s.layers.iter().find(|l| l.name == layer) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Tiles never change within a build, so a matching etag needs no body. It is
    // per archive, so re-baking one layer does not invalidate the others.
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == l.etag))
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    match l.archive.tile(z, x, y).map(<[u8]>::to_vec) {
        // Absent tiles are normal: a layer does not fill its bounding box, and
        // no layer reaches every rung.
        None => StatusCode::NO_CONTENT.into_response(),
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "application/vnd.mapbox-vector-tile"),
                // Stored gzipped; hand it over untouched.
                (header::CONTENT_ENCODING, "gzip"),
                (header::CACHE_CONTROL, "public, max-age=604800"),
                (header::ETAG, l.etag.as_str()),
            ],
            bytes,
        )
            .into_response(),
    }
}

/// The quoted string value for `key` in an archive's metadata blob.
fn json_string(meta: &str, key: &str) -> Option<String> {
    let rest = meta.split_once(key)?.1.trim_start().strip_prefix('"')?;
    rest.split_once('"').map(|(v, _)| v.to_string())
}

// --- anon zones -------------------------------------------------------------
// A position in, its k-anonymity zone out -- the same answer anon-serve gives,
// plus `quads`: the zone's cells as lon/lat boxes, which is what the viewer
// fills in so a click shows the anonymity set's true shape rather than its
// bounding box. See anon/README.md for what the answer means and the one rule
// every field obeys (a function of the zone alone, never of the position).

/// `GET /zone?lat=&lon=` -- for curl and for people. The POST form is the one
/// to build on: a URL lands in access logs, history and `Referer` headers by
/// default, and the URL is the thing being protected.
async fn zone_from_query(app: State<S>, RawQuery(query): RawQuery) -> Response {
    zone(&app, &query.unwrap_or_default())
}

/// `POST /zone` with `lat=..&lon=..` as the body.
async fn zone_from_body(app: State<S>, body: String) -> Response {
    zone(&app, &body)
}

/// `lat=..&lon=..`, by hand -- two splits over one short string, and nothing
/// here is echoed back or logged.
fn param(form: &str, name: &str) -> Option<f64> {
    form.split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)?
        .1
        .parse()
        .ok()
}

fn zone(s: &MapServer, form: &str) -> Response {
    let Some(anon) = &s.anon else {
        return zone_error(StatusCode::NOT_FOUND, "no zone index -- run `make anon`");
    };
    let (Some(lat), Some(lon)) = (param(form, "lat"), param(form, "lon")) else {
        return zone_error(StatusCode::BAD_REQUEST, "lat and lon must be numbers");
    };
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return zone_error(StatusCode::BAD_REQUEST, "lat and lon out of range");
    }
    if !anon.index.covers(lat, lon) {
        // The one thing this endpoint says about a position other than its
        // zone. See `Index::bounds`.
        return zone_error(StatusCode::NOT_FOUND, "outside the covered region");
    }
    let Some(z) = anon.index.zone(&anon.map, anon.tier, lat, lon) else {
        return zone_error(StatusCode::INTERNAL_SERVER_ERROR, "no zone");
    };
    // The zone's own JSON, with the quads spliced in before the closing brace.
    let mut body = z.to_json();
    body.pop();
    body.push_str(",\"quads\":[");
    for (i, q) in anon.index.quads(&z).iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(&format!("[{:.6},{:.6},{:.6},{:.6}]", q[0], q[1], q[2], q[3]));
    }
    body.push_str("]}");
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            // The mapping is a property of the index, not of the caller, so an
            // answer is cacheable -- but only where the position already is.
            // `private` keeps it out of shared caches; a CDN entry keyed on a
            // lat/lon URL is the access log this design refuses to write.
            (header::CACHE_CONTROL, "private, max-age=3600"),
        ],
        body,
    )
        .into_response()
}

fn zone_error(code: StatusCode, why: &str) -> Response {
    (
        code,
        [(header::CONTENT_TYPE, "application/json")],
        format!("{{\"error\":\"{why}\"}}"),
    )
        .into_response()
}
