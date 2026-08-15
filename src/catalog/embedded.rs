//! The manifest of catalogue files baked into the binary.
//!
//! This is the one place under `src/catalog/` where a CLI's name appears, and
//! it appears only as a file path: `include_str!` needs a literal, so the list
//! cannot be built from a directory scan without a build script. Nothing here
//! branches on which CLI it is -- adding one means dropping a file in
//! `catalog/` and adding a line below, never touching the code that reads it.
//!
//! Embedding rather than fetching is deliberate: code and data ship together,
//! so there is no schema-compatibility window between them and no runtime
//! supply-chain surface. Updating the catalogue is a release.

pub struct EmbeddedFile {
    /// Repo-relative path, carried so a parse failure can name the file the
    /// maintainer has to open.
    pub path: &'static str,
    pub source: &'static str,
}

/// Names each file exactly once; `include_str!` resolves relative to this
/// source file, hence the `../../` prefix back to the repo root.
macro_rules! embed {
    ($path:literal) => {
        EmbeddedFile {
            path: $path,
            source: include_str!(concat!("../../", $path)),
        }
    };
}

/// Every embedded entry, in the order they are offered to the user.
pub const FILES: &[EmbeddedFile] = &[embed!("catalog/claude.toml"), embed!("catalog/agy.toml")];
