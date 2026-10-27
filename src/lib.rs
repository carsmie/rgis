//! rGIS: a QGIS-Server-like OGC map server in Rust that serves QGIS
//! project files (.qgs/.qgz) over WMS, WFS and OGC API Features.
//!
//! The `rgis` binary is a thin command line around [`http::router`]; tests and tools can
//! drive the same [`Server`] directly.

// Ported QGIS/PAL functions keep their C++ signatures and container types.
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

mod crs;
mod expression;
mod geom;
pub mod http;
mod inspect;
mod labeling;
mod params;
mod postgis;
mod project;
mod provider;
mod raster;
mod render;
mod server;
mod services;
mod svg;
mod symbology;
mod text;

pub use inspect::inspect;
pub use server::{Request, Response, Server, Settings};
