//! Checkpointing: snapshot the document and its annotations on demand.
//!
//! Under `--checkpoint`, pressing `E` (send/export) also writes a numbered snapshot next to
//! the file, in a `.plannotator-checkpoint/` directory. Each snapshot is two files:
//!
//! - `<name>.v<N>` — a byte-for-byte copy of the document at that moment.
//! - `<name>.v<N>.meta` — JSON: the annotations and their computed line numbers.
//!
//! `N` increments from the highest existing snapshot, so each `E` is one checkpoint, made
//! when the reader is ready rather than on every change. `--diff` reads the two newest
//! snapshots; nothing here reads them back.

use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The directory snapshots live in, beside the document.
pub(crate) const DIR: &str = ".plannotator-checkpoint";

/// One annotation as a checkpoint records it: the body, its kind, the source byte range it
/// resolved to at checkpoint time, and the 1-based line that range started on. The byte
/// range is kept so a reader need not recompute it; the line number is the convenience the
/// spec asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MetaEntry {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) body: String,
    /// The raw-source text the annotation covered, for a reader that no longer has the
    /// exact bytes.
    pub(crate) quote: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
    /// 1-based line the range starts on, counted by newlines in the snapshot source.
    pub(crate) line: usize,
}

/// A checkpoint's metadata file: the document path it belongs to and its annotations.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct Meta {
    pub(crate) path: Option<PathBuf>,
    pub(crate) version: usize,
    pub(crate) entries: Vec<MetaEntry>,
}

/// One annotation handed to the checkpoint writer: resolved range, kind, body, quote.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) body: String,
    pub(crate) quote: String,
    pub(crate) range: Range<usize>,
}

/// The 1-based line `offset` sits on in `source`: one more than the newlines before it.
pub(crate) fn line_of(source: &str, offset: usize) -> usize {
    let at = offset.min(source.len());
    source.get(..at).map_or(1, |s| s.bytes().filter(|&b| b == b'\n').count() + 1)
}

/// The snapshot directory for `doc_path` (its parent, plus [`DIR`]).
pub(crate) fn dir_for(doc_path: &Path) -> PathBuf {
    doc_path.parent().unwrap_or_else(|| Path::new(".")).join(DIR)
}

/// The file name the document has, for building snapshot names. Falls back to `document`.
fn base_name(doc_path: &Path) -> String {
    doc_path.file_name().map_or_else(|| "document".to_owned(), |n| n.to_string_lossy().into_owned())
}

/// The highest `N` already written in `dir` for `base`, or 0 when there are none. A
/// snapshot is `<base>.v<N>`; its meta is `<base>.v<N>.meta`, which is not counted.
fn highest_version(dir: &Path, base: &str) -> usize {
    let prefix = format!("{base}.v");
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|name| {
            let rest = name.strip_prefix(&prefix)?;
            // Skip the `.meta` sidecars; only the bare `<base>.v<N>` counts.
            (!rest.contains('.')).then(|| rest.parse::<usize>().ok()).flatten()
        })
        .max()
        .unwrap_or(0)
}

/// The outcome of a checkpoint attempt: a freshly written version, or the existing latest
/// version when the state was byte-identical to it (so repeated `E` with no change does not
/// pile up duplicates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Written {
    New(usize),
    Unchanged(usize),
}

/// The meta entries `source` and `entries` would produce, for comparing against an existing
/// checkpoint without writing.
fn meta_entries(source: &str, entries: &[Entry]) -> Vec<MetaEntry> {
    entries
        .iter()
        .map(|e| MetaEntry {
            id: e.id.clone(),
            kind: e.kind.clone(),
            body: e.body.clone(),
            quote: e.quote.clone(),
            start: e.range.start,
            end: e.range.end,
            line: line_of(source, e.range.start),
        })
        .collect()
}

/// True when the latest snapshot `v<version>` under `dir` holds `source` with the same
/// `entries` meta. The ids differ across sessions, so they are ignored in the comparison.
fn same_as_latest(dir: &Path, base: &str, version: usize, source: &str, entries: &[MetaEntry]) -> bool {
    if version == 0 {
        return false;
    }
    let snapshot = dir.join(format!("{base}.v{version}"));
    if std::fs::read_to_string(&snapshot).ok().as_deref() != Some(source) {
        return false;
    }
    let meta_path = dir.join(format!("{base}.v{version}.meta"));
    let Ok(text) = std::fs::read_to_string(&meta_path) else { return false };
    let Ok(meta) = serde_json::from_str::<Meta>(&text) else { return false };
    let ignoring_id = |e: &MetaEntry| (e.kind.clone(), e.body.clone(), e.quote.clone(), e.start, e.end, e.line);
    meta.entries.iter().map(ignoring_id).eq(entries.iter().map(ignoring_id))
}

/// Write the next checkpoint for `doc_path`: a copy of `source` and a `.meta` of `entries`.
/// Returns [`Written::New`] with the version written, or [`Written::Unchanged`] when the
/// state is byte-identical to the latest snapshot (no new version is written). `entries`
/// are expected in source order; the meta records each one's start line in `source`.
pub(crate) fn write(doc_path: &Path, source: &str, entries: &[Entry]) -> Result<Written> {
    let dir = dir_for(doc_path);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let base = base_name(doc_path);
    let latest = highest_version(&dir, &base);

    let computed = meta_entries(source, entries);
    if same_as_latest(&dir, &base, latest, source, &computed) {
        return Ok(Written::Unchanged(latest));
    }
    let version = latest + 1;

    let snapshot = dir.join(format!("{base}.v{version}"));
    std::fs::write(&snapshot, source).with_context(|| format!("writing {}", snapshot.display()))?;

    let meta = Meta { path: Some(doc_path.to_path_buf()), version, entries: computed };
    let meta_path = dir.join(format!("{base}.v{version}.meta"));
    let json = serde_json::to_string_pretty(&meta)?;
    std::fs::write(&meta_path, json).with_context(|| format!("writing {}", meta_path.display()))?;
    Ok(Written::New(version))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, reason = "tests assert by panicking")]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plannotator-tui-ckpt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn line_of_counts_newlines_before_the_offset() {
        let src = "a\nbb\nccc\n";
        assert_eq!(line_of(src, 0), 1);
        assert_eq!(line_of(src, 2), 2); // first byte of "bb"
        assert_eq!(line_of(src, 5), 3); // first byte of "ccc"
        assert_eq!(line_of(src, 999), 4); // past the end clamps
    }

    #[test]
    fn write_increments_the_version_and_records_lines() {
        let root = temp_dir("write");
        let doc = root.join("test.md");
        let source = "# A\n\nsecond line\n";
        let start = source.find("second").expect("present");
        let entries =
            vec![Entry { id: "x".into(), kind: "comment".into(), body: "note".into(), quote: "second".into(), range: start..start + 6 }];

        let v1 = write(&doc, source, &entries).expect("write v1");
        assert_eq!(v1, Written::New(1));
        let dir = dir_for(&doc);
        assert!(dir.join("test.md.v1").is_file());
        assert!(dir.join("test.md.v1.meta").is_file());
        assert_eq!(std::fs::read_to_string(dir.join("test.md.v1")).expect("snapshot"), source);

        let meta: Meta =
            serde_json::from_str(&std::fs::read_to_string(dir.join("test.md.v1.meta")).expect("meta"))
                .expect("parse");
        assert_eq!(meta.version, 1);
        assert_eq!(meta.entries.len(), 1);
        assert_eq!(meta.entries[0].body, "note");
        assert_eq!(meta.entries[0].line, 3, "the comment is on line 3");

        // Same state again: deduped, no new version.
        let again = write(&doc, source, &entries).expect("write again");
        assert_eq!(again, Written::Unchanged(1), "identical state does not bump the version");
        assert!(!dir.join("test.md.v2").exists(), "no duplicate v2");

        // A changed body writes v2 and leaves v1 alone.
        let mut changed = entries.clone();
        changed[0].body = "edited".into();
        let v2 = write(&doc, source, &changed).expect("write v2");
        assert_eq!(v2, Written::New(2));
        assert!(dir.join("test.md.v2").is_file());
        assert!(dir.join("test.md.v1").is_file(), "v1 is kept");

        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
