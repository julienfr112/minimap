//! What the router promises over HTTP, checked without a socket.
//!
//! Every status code the viewer branches on is asserted here, because the
//! viewer treats them as facts: 204 is "nothing there", 404 is "no such
//! layer", 304 is "keep what you have". Silent drift in any of them shows up
//! as a blank map, not as an error -- an absent tile and a 404 are the same
//! thing to a renderer.
//!
//! The shell's `<base href>` is here for the same reason: it is what makes
//! every other viewer request relative, so if it stops naming the prefix the
//! map goes blank under a nest and nowhere else.

mod fixture;

use std::path::{Path, PathBuf};

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use minimap_server::{builds, MapServer, Options};
use tower::ServiceExt;

use fixture::Fixture;

const ROADS_SALT: u64 = 0x9E37;

/// A directory of two small archives of one build, one router over it.
fn setup(name: &str) -> (PathBuf, Fixture, Fixture) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let roads = Fixture::write(&dir.join("test.roads.pmtiles"), 3, ROADS_SALT);
    let land = Fixture::write(&dir.join("test.land.pmtiles"), 1, 0x2545);
    (dir, roads, land)
}

fn options(dir: &Path, name: &str) -> Options {
    Options {
        tiles: dir.to_path_buf(),
        name: name.to_string(),
        zones: None,
        k: None,
    }
}

fn open(dir: &Path) -> Router {
    MapServer::open(&options(dir, "test")).unwrap().router()
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: header::HeaderName) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

async fn get(app: &Router, path: &str) -> Reply {
    send(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn send(app: &Router, req: Request<Body>) -> Reply {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

#[tokio::test]
async fn the_viewer_is_compiled_in() {
    let (dir, ..) = setup("routes-viewer");
    let app = open(&dir);

    let shell = get(&app, "/").await;
    assert_eq!(shell.status, StatusCode::OK);
    assert_eq!(
        shell.header(header::CONTENT_TYPE),
        Some("text/html; charset=utf-8")
    );
    assert!(shell.text().contains(r#"<script src="minimap.js">"#));

    let js = get(&app, "/minimap.js").await;
    assert_eq!(js.status, StatusCode::OK);
    assert_eq!(
        js.header(header::CONTENT_TYPE),
        Some("text/javascript; charset=utf-8")
    );
    assert!(js.text().contains("class Minimap"));

    // Nothing else is served from disk: there is no asset route to climb.
    assert_eq!(get(&app, "/index.html").await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        get(&app, "/../Cargo.toml").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn meta_describes_the_archives_that_exist() {
    let (dir, _, _) = setup("routes-meta");
    let app = open(&dir);
    let meta = get(&app, "/meta.json").await;
    assert_eq!(meta.status, StatusCode::OK);
    assert_eq!(meta.header(header::CONTENT_TYPE), Some("application/json"));
    let text = meta.text();

    // Layers in draw order, each with the rungs its archive declares.
    let land_at = text.find(r#""name":"land""#).expect("land listed");
    let roads_at = text.find(r#""name":"roads""#).expect("roads listed");
    assert!(
        land_at < roads_at,
        "land must be listed before roads: {text}"
    );
    assert!(text.contains(r#""rungs":[0,1,2,3]"#), "{text}");
    assert!(text.contains(r#""rungs":[0,1]"#), "{text}");
    // The map's range is the union of its layers'.
    assert!(text.contains(r#""minzoom":0,"maxzoom":3"#), "{text}");
    // No index was given, so the viewer is not offered a zone click.
    assert!(!text.contains(r#""anon""#), "{text}");
    assert!(
        text.contains("synthetic"),
        "attribution comes from the archive"
    );
}

#[tokio::test]
async fn tiles_come_back_as_stored_or_as_the_right_absence() {
    let (dir, roads, _) = setup("routes-tiles");
    let app = open(&dir);

    let (z, x, y) = (3u8, 5u32, 2u32);
    let tile = get(&app, &format!("/tiles/roads/{z}/{x}/{y}")).await;
    assert_eq!(tile.status, StatusCode::OK);
    assert_eq!(
        tile.header(header::CONTENT_TYPE),
        Some("application/vnd.mapbox-vector-tile")
    );
    assert_eq!(tile.header(header::CONTENT_ENCODING), Some("gzip"));
    assert_eq!(
        tile.header(header::CACHE_CONTROL),
        Some("public, max-age=604800")
    );
    // Byte for byte what the archive holds: the server neither inflates nor
    // re-encodes.
    assert_eq!(tile.body, roads.stored(z, x, y, ROADS_SALT));

    // Past the layer's deepest rung: nothing there, and not an error.
    assert_eq!(
        get(&app, "/tiles/roads/4/0/0").await.status,
        StatusCode::NO_CONTENT
    );
    // Off the grid at a rung that exists: same answer.
    assert_eq!(
        get(&app, "/tiles/roads/2/4/0").await.status,
        StatusCode::NO_CONTENT
    );
    // A zoom the format cannot hold at all.
    assert_eq!(
        get(&app, "/tiles/roads/40/0/0").await.status,
        StatusCode::NO_CONTENT
    );
    // A layer this build never produced is final in a different way.
    assert_eq!(
        get(&app, "/tiles/rivers/0/0/0").await.status,
        StatusCode::NOT_FOUND
    );
    // Garbage in the path is the router's problem, not a panic.
    assert!(get(&app, "/tiles/roads/x/0/0")
        .await
        .status
        .is_client_error());
    assert!(get(&app, "/tiles/roads/300/0/0")
        .await
        .status
        .is_client_error());
}

#[tokio::test]
async fn etags_revalidate_per_archive() {
    let (dir, ..) = setup("routes-etag");
    let app = open(&dir);
    let path = "/tiles/roads/1/0/0";
    let first = get(&app, path).await;
    let etag = first.header(header::ETAG).expect("etag").to_string();
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "quoted: {etag}"
    );

    let again = send(
        &app,
        Request::get(path)
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert!(again.body.is_empty());

    // Another layer's etag says nothing about this one.
    let land = get(&app, "/tiles/land/1/0/0").await;
    let other = land.header(header::ETAG).unwrap().to_string();
    assert_ne!(etag, other);
    let cross = send(
        &app,
        Request::get(path)
            .header(header::IF_NONE_MATCH, &other)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(cross.status, StatusCode::OK);
}

/// axum's `nest("/map", ..)` routes `/map` and `/map/anything`, but not
/// `/map/` -- so the shell cannot rely on a trailing slash and instead carries
/// a `<base href>` naming the prefix it was reached under.
#[tokio::test]
async fn nested_under_a_prefix_the_shell_names_its_base() {
    let (dir, ..) = setup("routes-nested");
    let app = Router::new().nest("/map", open(&dir));

    let shell = get(&app, "/map").await;
    assert_eq!(shell.status, StatusCode::OK, "{:?}", shell.headers);
    assert!(
        shell.text().contains(r#"<base href="/map/">"#),
        "{}",
        shell.text()
    );

    // The query string is the viewer's (`?maxzoom=`) and must not disturb it.
    let with_query = get(&app, "/map?maxzoom=2").await;
    assert_eq!(with_query.status, StatusCode::OK);
    assert!(with_query.text().contains(r#"<base href="/map/">"#));

    // At the root the base is the root.
    let root = get(&open(&dir), "/").await;
    assert!(
        root.text().contains(r#"<base href="/">"#),
        "{}",
        root.text()
    );

    // Everything the shell asks for is relative to that base, so it lands
    // under the prefix.
    assert_eq!(get(&app, "/map/minimap.js").await.status, StatusCode::OK);
    assert_eq!(get(&app, "/map/meta.json").await.status, StatusCode::OK);
    assert_eq!(
        get(&app, "/map/tiles/roads/0/0/0").await.status,
        StatusCode::OK
    );
    assert_eq!(
        get(&app, "/map/zone?lat=1&lon=1").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn zone_without_an_index_says_so() {
    let (dir, ..) = setup("routes-zone");
    let app = open(&dir);
    let r = get(&app, "/zone?lat=49.89&lon=2.30").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.header(header::CONTENT_TYPE), Some("application/json"));
    assert!(r.text().contains("make anon"), "{}", r.text());

    let posted = send(
        &app,
        Request::post("/zone")
            .body(Body::from("lat=49.89&lon=2.30"))
            .unwrap(),
    )
    .await;
    assert_eq!(posted.status, StatusCode::NOT_FOUND);
}

#[test]
fn no_archives_is_a_refusal_not_an_empty_map() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("routes-empty");
    std::fs::create_dir_all(&dir).unwrap();
    // A file that is not <name>.<layer>.pmtiles is not a build.
    std::fs::write(dir.join("notes.txt"), b"x").unwrap();
    std::fs::write(dir.join("roads.pmtiles"), b"old naming").unwrap();
    let err = MapServer::open(&options(&dir, "test"))
        .err()
        .expect("an empty directory must not open");
    assert!(err.to_string().contains("make all"), "{err}");
    assert_eq!(builds(&dir).unwrap(), Vec::<String>::new());

    let err = MapServer::open(&options(Path::new("/nonexistent/minimap-pmtiles"), "test"))
        .err()
        .expect("a missing directory must not open");
    assert!(err.to_string().contains("make all"), "{err}");
}

/// Several builds share one directory; the server opens exactly the one it
/// was asked for, and names the others when asked for one that is not there.
#[test]
fn builds_share_a_directory_and_are_opened_by_name() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("routes-builds");
    Fixture::write(&dir.join("europe.roads.pmtiles"), 2, 1);
    Fixture::write(&dir.join("europe.land.pmtiles"), 1, 2);
    Fixture::write(&dir.join("picardie.roads.pmtiles"), 1, 3);
    assert_eq!(
        builds(&dir).unwrap(),
        vec!["europe".to_string(), "picardie".to_string()]
    );

    let europe = MapServer::open(&options(&dir, "europe")).unwrap();
    assert!(
        europe.report().starts_with("  build europe\n"),
        "{}",
        europe.report()
    );
    assert_eq!(
        europe.report().matches(" tiles ").count(),
        2,
        "{}",
        europe.report()
    );
    let picardie = MapServer::open(&options(&dir, "picardie")).unwrap();
    assert_eq!(picardie.report().matches(" tiles ").count(), 1);

    let err = MapServer::open(&options(&dir, "asia"))
        .err()
        .expect("no such build");
    let msg = err.to_string();
    assert!(msg.contains("europe") && msg.contains("picardie"), "{msg}");
}
