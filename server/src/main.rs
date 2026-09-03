//! The standalone minimap server: the library plus an environment.
//!
//! Everything that guesses lives here -- paths relative to this checkout, the
//! env vars `make serve` sets -- so the library can take explicit paths and
//! nothing else. See src/lib.rs, and server/README.md for mounting the same
//! router inside another application.
//!
//!   MINIMAP_TILES  directory of <name>.<layer>.pmtiles   (default ../pmtiles)
//!   MINIMAP_NAME   which build to serve                  (default: the only one there)
//!   MINIMAP_PORT   listen port                           (default 8090)
//!   ANON_INDEX     the zone index                        (default ../anon/<name>.anon-zones.bin)
//!   ANON_K         which tier /zone answers from         (default: the most private baked)
//!
//! The viewer is compiled in, so deploying is this binary, the archives, and
//! (optionally) the zone index:
//! `MINIMAP_TILES=/srv/pmtiles MINIMAP_NAME=europe minimap-backend`.
//! The defaults below only make sense inside the checkout, for `make serve`.

use std::{net::SocketAddr, path::PathBuf};

use minimap_server::{builds, MapServer, Options};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The archives are build output and live at the top level of the checkout,
    // one directory up from this crate.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("server/ has a parent")
        .to_path_buf();

    let env_path =
        |name: &str, default: PathBuf| std::env::var(name).map(PathBuf::from).unwrap_or(default);
    let tiles = env_path("MINIMAP_TILES", root.join("pmtiles"));

    // Unnamed, serve the one build there is; several is a choice nobody
    // should have made silently.
    let name = match std::env::var("MINIMAP_NAME") {
        Ok(name) => name,
        Err(_) => {
            let found = builds(&tiles)
                .map_err(|e| format!("{}: {e} -- run `make all` first", tiles.display()))?;
            match found.as_slice() {
                [one] => one.clone(),
                [] => {
                    return Err(format!(
                        "no <name>.<layer>.pmtiles in {} -- run `make all` first",
                        tiles.display()
                    )
                    .into())
                }
                many => {
                    return Err(format!(
                    "several builds in {}: {} -- say which with MINIMAP_NAME (make serve NAME=...)",
                    tiles.display(),
                    many.join(", ")
                )
                    .into())
                }
            }
        }
    };

    let opts = Options {
        zones: Some(env_path(
            "ANON_INDEX",
            root.join(format!("anon/{name}.anon-zones.bin")),
        )),
        tiles,
        name,
        k: match std::env::var("ANON_K") {
            Ok(k) => Some(k.parse()?),
            Err(_) => None,
        },
    };

    let server = MapServer::open(&opts)?;
    print!("{}", server.report());

    let port: u16 = match std::env::var("MINIMAP_PORT") {
        Ok(p) => p.parse()?,
        Err(_) => 8090,
    };
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("listening on http://{addr}");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, server.router()).await?;
    Ok(())
}
