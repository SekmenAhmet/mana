//! The manifest of catalogue files baked into the binary.
//!
//! This is the one place under `src/catalog/` where a CLI's name appears, and
//! it appears only as a file path: `include_str!` needs a literal, so the list
//! cannot be built from a directory scan without a build script. Nothing here
//! branches on which CLI it is -- adding one means dropping a file in
//! `catalog/` and adding a line below, never touching the code that reads it.
//! Forgetting the second half is caught by the test at the bottom of this file,
//! which reads `catalog/` and compares.
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
pub const FILES: &[EmbeddedFile] = &[
    embed!("catalog/claude.toml"),
    embed!("catalog/agy.toml"),
    embed!("catalog/copilot.toml"),
    embed!("catalog/opencode.toml"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The half of "drop a file in `catalog/` and add a line below" that the
    /// compiler cannot check: `include_str!` fails loudly on a line naming a
    /// file that does not exist, but a file with no line is silent -- it is
    /// committed, reviewed, and simply absent from the binary. The only
    /// symptom is a CLI `mana install` never offers.
    ///
    /// A directory read rather than a `build.rs` that generates this list:
    /// both catch the omission at the same moment (the PR that adds the file),
    /// but a build script means an OUT_DIR include, a `rerun-if-changed`, and
    /// a codegen step on every build of every consumer -- machinery for a
    /// guarantee ten lines of test already give.
    #[test]
    fn every_catalogue_file_on_disk_is_embedded() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/catalog");
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .expect("catalog/ exists")
            .map(|entry| entry.expect("readable entry").path())
            // Non-recursive, so `catalog/goldens/` is skipped as a directory;
            // the extension filter keeps a stray README out of the comparison.
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .map(|path| {
                format!(
                    "catalog/{}",
                    path.file_name()
                        .expect("a file has a name")
                        .to_string_lossy()
                )
            })
            .collect();
        on_disk.sort();

        let mut embedded: Vec<String> = FILES.iter().map(|f| f.path.to_string()).collect();
        embedded.sort();

        assert_eq!(
            embedded, on_disk,
            "catalog/ and the FILES manifest disagree -- a file here without an `embed!` line \
             ships in the repo but not in the binary"
        );
    }
}
