//! minimap backend.
//!
//! Serves a single PMTiles archive over plain `/tiles/{z}/{x}/{y}`, so the
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

struct App {
    archive: Archive,
    web: PathBuf,
    /// Content of the archive is immutable, so one etag for the whole build is
    /// enough to let browsers skip re-downloading tiles they already have.
    etag: String,
}

type S = Arc<App>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("backend/ has a parent")
        .to_path_buf();

    let path = std::env::var("MINIMAP_ARCHIVE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("minimap.pmtiles"));
    if !path.exists() {
        return Err(format!(
            "{} not found -- run `minimap all` (or `minimap export`) first",
            path.display()
        )
        .into());
    }

    let archive = Archive::open(&path)?;
    // A build fingerprint: archive size plus mtime, enough to change whenever a
    // new archive is shipped.
    let meta = std::fs::metadata(&path)?;
    let etag = format!(
        "\"{:x}-{:x}\"",
        meta.len(),
        meta.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs()
    );
    println!(
        "{} tiles, z{}..{}, {:.1} MB archive: {}",
        archive.tile_count,
        archive.min_zoom,
        archive.max_zoom,
        meta.len() as f64 / 1e6,
        path.display()
    );

    let app = Router::new()
        .route("/", get(index))
        .route("/meta.json", get(meta_json))
        .route("/tiles/{z}/{x}/{y}", get(tile))
        .route("/{*path}", get(asset))
        .with_state(Arc::new(App { archive, web: root.join("web"), etag }));

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

/// TileJSON, read straight out of the archive header so the viewer never
/// hardcodes the zoom range or extent of whatever region was baked.
async fn meta_json(State(s): State<S>) -> Response {
    let a = &s.archive;
    let attribution = a
        .metadata
        .split_once("\"attribution\":")
        .and_then(|(_, rest)| rest.trim_start().strip_prefix('"'))
        .and_then(|rest| rest.split_once('"'))
        .map(|(v, _)| v.to_string())
        .unwrap_or_else(|| "© OpenStreetMap contributors".into());
    let body = format!(
        r#"{{"tilejson":"3.0.0","scheme":"xyz","tiles":["/tiles/{{z}}/{{x}}/{{y}}"],
"minzoom":{},"maxzoom":{},"bounds":[{},{},{},{}],"center":[{},{},{}],
"attribution":"{}"}}"#,
        a.min_zoom,
        a.max_zoom,
        a.min_lon,
        a.min_lat,
        a.max_lon,
        a.max_lat,
        (a.min_lon + a.max_lon) / 2.0,
        (a.min_lat + a.max_lat) / 2.0,
        a.center_zoom,
        attribution,
    );
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn tile(
    State(s): State<S>,
    Path((z, x, y)): Path<(u8, u32, u32)>,
    headers: HeaderMap,
) -> Response {
    // Tiles never change within a build, so a matching etag needs no body.
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == s.etag))
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    match s.archive.tile(z, x, y).map(<[u8]>::to_vec) {
        // Absent tiles are normal: the region does not fill its bounding box.
        None => StatusCode::NO_CONTENT.into_response(),
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "application/vnd.mapbox-vector-tile"),
                // Stored gzipped; hand it over untouched.
                (header::CONTENT_ENCODING, "gzip"),
                (header::CACHE_CONTROL, "public, max-age=604800"),
                (header::ETAG, s.etag.as_str()),
            ],
            bytes,
        )
            .into_response(),
    }
}
