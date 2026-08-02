//! minimap backend.
//!
//! Serves one PMTiles archive per layer over plain `/tiles/{layer}/{z}/{x}/{y}`, so the
//! viewer needs no range-request logic and no CDN is involved.
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

mod pmtiles;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

use pmtiles::Archive;

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
}

struct App {
    /// In draw order, which is also the order the viewer is told about them.
    layers: Vec<Layer>,
    web: PathBuf,
}

/// Draw order. The viewer has its own copy for styling, but the server decides
/// what order it hears about them in, so an archive dropped in by hand still
/// lands in the right place.
const ORDER: [&str; 6] = ["land", "landuse", "water", "roads", "buildings", "places"];

type S = Arc<App>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The viewer ships with the server that serves it, so it is found relative
    // to this crate. The archives are build output and live at the top level.
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = here.parent().expect("server/ has a parent").to_path_buf();

    let dir = std::env::var("MINIMAP_TILES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("pmtiles"));

    // Whatever archives are there. Which layers a build produced is a fact about
    // the build, not a list to keep in step by hand: a layer with no tiles gets
    // no archive, and this simply does not find one.
    let mut layers: Vec<Layer> = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e} -- run `make all` first", dir.display()))?
        .flatten()
    {
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = file.strip_suffix(".pmtiles") else {
            continue;
        };
        let path = entry.path();
        let archive = Archive::open(&path)?;
        let meta = entry.metadata()?;
        // A build fingerprint: size plus mtime, enough to change whenever a new
        // archive is shipped.
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
        println!(
            "  {name:<12} {:>9} tiles  z{}..{}  {:>8.1} MB",
            archive.tile_count,
            archive.min_zoom,
            archive.max_zoom,
            meta.len() as f64 / 1e6
        );
        layers.push(Layer {
            name: name.to_string(),
            archive,
            rungs,
            etag,
        });
    }
    if layers.is_empty() {
        return Err(format!("no .pmtiles in {} -- run `make all` first", dir.display()).into());
    }
    layers.sort_by_key(|l| ORDER.iter().position(|o| *o == l.name).unwrap_or(usize::MAX));
    println!("{} layers from {}", layers.len(), dir.display());

    let app = Router::new()
        .route("/", get(index))
        .route("/meta.json", get(meta_json))
        .route("/tiles/{layer}/{z}/{x}/{y}", get(tile))
        .route("/{*path}", get(asset))
        .with_state(Arc::new(App { layers, web: here.join("web") }));

    let port: u16 = std::env::var("MINIMAP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8090);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("listening on http://{addr}");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}

async fn index(State(s): State<S>) -> Response {
    serve_file(&s.web.join("index.html"), "text/html; charset=utf-8").await
}

/// Static assets from web/. No directory listing, and any path that could climb
/// out of web/ is refused rather than normalised.
async fn asset(State(s): State<S>, Path(path): Path<String>) -> Response {
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

async fn serve_file(path: &std::path::Path, content_type: &'static str) -> Response {
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
        r#"{{"tilejson":"3.0.0","scheme":"xyz","tiles":["/tiles/{{layer}}/{{z}}/{{x}}/{{y}}"],
"minzoom":{lo},"maxzoom":{hi},"bounds":[{w},{sth},{e},{n}],"center":[{cx},{cy},{lo}],
"layers":[
  {layers}
],
"attribution":"{attribution}"}}"#,
        cx = (w + e) / 2.0,
        cy = (sth + n) / 2.0,
    );
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn tile(
    State(s): State<S>,
    Path((layer, z, x, y)): Path<(String, u8, u32, u32)>,
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
