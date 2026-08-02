//! The pipeline's steps, exposed as a library.
//!
//! The binary in main.rs is the only intended caller, and `make` is the only
//! intended caller of *that*. This is a library rather than one flat binary so
//! the split between the two kinds of configuration stays visible and testable:
//!
//!   * [`tuning`] is what the map *is* — layers, classes, size thresholds, the
//!     SQL derived from them. Changing it changes the map.
//!   * [`config`] is what this *run* is doing — which directories, which zooms.
//!     Changing it changes nothing about the map, only where it lands.
//!
//! Everything a step needs from the second arrives as a `&Config` argument, so
//! nothing here reads an environment variable or infers a path from where the
//! binary was built. That is what makes the build directory a flag.

pub mod bake;
pub mod config;
pub mod download;
pub mod export;
pub mod extract;
pub mod geom;
pub mod info;
pub mod load;
pub mod progress;
pub mod rows;
pub mod sql;
pub mod tuning;
