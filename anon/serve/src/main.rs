//! The anonymising service: a position in, a zone out.
//!
//!   POST /zone            lat=49.8949&lon=2.3020
//!   GET  /zone?lat=49.8949&lon=2.3020
//!
//!   {"zone":"d99798fa5","k":64,"buildings":69,
//!    "bbox":[2.300262,49.894634,2.307129,49.898173],"center":[2.303696,49.896404],
//!    "radius_m":315,"area_km2":0.116,"cells":12,
//!    "density_per_km2":484,"built_index":35.34,"kind":"city"}
//!
//! There is no database, no geometry library and no projection code beyond one
//! `asinh`. The zones were cut offline; a request is a projection, a Hilbert
//! index and one binary search over an mmapped array, so the answer costs a page
//! fault and nothing else.
//!
//! ## Operating it
//!
//!   ANON_INDEX  path to the baked index      (default anon/anon-zones.bin)
//!   ANON_K      which tier to answer from    (default: the largest baked)
//!   ANON_ADDR   listen address               (default 127.0.0.1:8091)
//!
//! `k` is deliberately not a query parameter. Tiers nest -- a position's k=16
//! zone sits inside its k=256 zone -- so letting the caller choose means anyone
//! who asks twice keeps the smaller answer, and the effective k for the whole
//! service is the minimum on the menu. One k per deployment; run a second
//! instance if a second use case needs a different one.
//!
//! ## What this process must never log
//!
//! The request line. `lat`/`lon` are the thing being protected, and an access log
//! is a durable, greppable, backed-up record of exactly the positions the API
//! exists to avoid handing on. Nothing here writes a request to disk, and a
//! reverse proxy in front of it has to be configured not to either -- which is
//! the one thing about this design that lives outside this repository.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anon_format::{Index, Zone};
use axum::{
    extract::{RawQuery, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use memmap2::Mmap;

struct App {
    map: Mmap,
    index: Index,
    /// Resolved once at startup: the request path never chooses a tier.
    tier: usize,
}

type S = Arc<App>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("anon/serve/ has two parents")
        .to_path_buf();
    let path = std::env::var("ANON_INDEX")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("anon/anon-zones.bin"));
    if !path.exists() {
        return Err(format!("{} not found -- run `anon-bake` first", path.display()).into());
    }

    let file = std::fs::File::open(&path)?;
    // SAFETY: the index is immutable for the life of the process. A re-bake
    // writes a new file and the service is restarted onto it, exactly as the
    // tile archive is handled.
    let map = unsafe { Mmap::map(&file)? };
    let index = Index::parse(&map)?;

    let wanted = match std::env::var("ANON_K") {
        Ok(k) => Some(k.parse::<u32>()?),
        Err(_) => None,
    };
    let tier = index.tier(wanted).ok_or_else(|| {
        format!(
            "no tier for k={:?}; baked tiers are {:?}",
            wanted,
            index.tiers.iter().map(|t| t.k).collect::<Vec<_>>()
        )
    })?;
    let t = index.tiers[tier];
    println!(
        "k={}, {} zones at z{}, {:.1} MB index, bounds {:.2} {:.2} .. {:.2} {:.2}: {}",
        t.k,
        t.zones,
        index.level,
        map.len() as f64 / 1e6,
        index.bounds[0],
        index.bounds[1],
        index.bounds[2],
        index.bounds[3],
        path.display()
    );

    let addr: SocketAddr = std::env::var("ANON_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8091".into())
        .parse()?;
    let app = Router::new()
        .route("/zone", get(from_query).post(from_body))
        .with_state(Arc::new(App { map, index, tier }));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("listening on http://{addr}/zone?lat=49.894&lon=2.298");
    axum::serve(listener, app).await?;
    Ok(())
}

/// `lat=..&lon=..`, by hand.
///
/// `Query<T>` would want a `Deserialize` and would put the offending value in its
/// own rejection body; two splits over one short string are cheaper than either.
/// Nothing here is echoed back.
fn param(form: &str, name: &str) -> Option<f64> {
    form.split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)?
        .1
        .parse()
        .ok()
}

/// `GET /zone?lat=&lon=`. Convenient, and the wrong shape for production: a URL
/// ends up in access logs, browser history, `Referer` headers and cache keys by
/// default, and the URL is the thing being protected. Kept because it is what a
/// person types into curl.
async fn from_query(app: State<S>, RawQuery(query): RawQuery) -> Response {
    zone(app, &query.unwrap_or_default())
}

/// `POST /zone` with `lat=..&lon=..` as the body -- same parse, but nothing
/// downstream logs a request body without being asked to. The shape to deploy.
async fn from_body(app: State<S>, body: String) -> Response {
    zone(app, &body)
}

fn zone(State(app): State<S>, form: &str) -> Response {
    let (Some(lat), Some(lon)) = (param(form, "lat"), param(form, "lon")) else {
        return bad(StatusCode::BAD_REQUEST, "lat and lon must be numbers");
    };
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return bad(StatusCode::BAD_REQUEST, "lat and lon out of range");
    }
    if !app.index.covers(lat, lon) {
        // The one thing this service says about a position other than its zone.
        // See `Index::bounds`.
        return bad(StatusCode::NOT_FOUND, "outside the covered region");
    }
    match app.index.zone(&app.map, app.tier, lat, lon) {
        Some(z) => ok(&z),
        None => bad(StatusCode::INTERNAL_SERVER_ERROR, "no zone"),
    }
}

fn ok(zone: &Zone) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            // The mapping is a property of the index, not of the caller, so an
            // answer is cacheable -- but only where the *position* already is.
            // `private` keeps it out of shared caches; a CDN entry keyed on a
            // lat/lon URL is the access log this design refuses to write.
            (header::CACHE_CONTROL, "private, max-age=3600"),
        ],
        zone.to_json(),
    )
        .into_response()
}

fn bad(code: StatusCode, why: &str) -> Response {
    (
        code,
        [(header::CONTENT_TYPE, "application/json")],
        format!("{{\"error\":\"{why}\"}}"),
    )
        .into_response()
}
