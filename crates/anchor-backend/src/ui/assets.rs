//! The dashboard's static files, embedded at build time from `ui/dist`
//! (see `build.rs`). Served from this fixed table only: there is no
//! filesystem lookup, so no path can escape it.

include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));
