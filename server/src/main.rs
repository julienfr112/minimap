//! The standalone minimap server: the library plus an environment.
//!
//! Everything that guesses lives here -- paths relative to this checkout, the
//! env vars `make serve` sets -- so the library can take explicit paths and
//! nothing else. See src/lib.rs, and server/README.md for mounting the same
//! router inside another application.
//!
//!   MINIMAP_TILES  directory of .pmtiles archives   (default ../pmtiles)
//!   MINIMAP_PORT   listen port                      (default 8090)
//!   ANON_INDEX     the zone index                   (default ../anon/anon-zones.bin)
//!   ANON_K         which tier /zone answers from    (default: the most private baked)

use std::{net::SocketAddr, path::PathBuf};

use minimap_server::{MapServer, Options};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The viewer ships with the server that serves it, so it is found relative
    // to this crate. The archives are build output and live at the top level.
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = here.parent().expect("server/ has a parent").to_path_buf();

    let env_path = |name: &str, default: PathBuf| {
        std::env::var(name).map(PathBuf::from).unwrap_or(default)
    };
    let opts = Options {
        tiles: env_path("MINIMAP_TILES", root.join("pmtiles")),
        web: here.join("web"),
        zones: Some(env_path("ANON_INDEX", root.join("anon/anon-zones.bin"))),
        k: match std::env::var("ANON_K") {
            Ok(k) => Some(k.parse()?),
            Err(_) => None,
        },
    };

    let server = MapServer::open(&opts)?;
    print!("{}", server.report());

    let port: u16 = std::env::var("MINIMAP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8090);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("listening on http://{addr}");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, server.router()).await?;
    Ok(())
}
