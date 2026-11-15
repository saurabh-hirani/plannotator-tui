//! `--diff`: a read-only before/after of the two newest checkpoints.
//!
//! Reads the two highest `v<N>` snapshots under `.plannotator-checkpoint/` (the newest is
//! "after", the one before it is "before") and diffs them by Markdown block. [`build`]
//! returns a structured [`Diff`]; [`render`] turns it into a synthetic Markdown document for
//! a chosen [`Layout`], with the byte ranges to word-highlight and the comments to anchor.
//!
//! - Stacked: each change as a `[before]` section then an `[after]` section.
//! - Unified: each change as one block with `-` removed and `+` added lines.
//!
//! Within a change, a word-level diff marks the removed words (in the before text) and the
//! added words (in the after text) so the reader sees exactly what moved. The synthetic
//! document is opened read-only; nothing here writes.

use std::fmt::Write as _;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use similar::{Algorithm, DiffOp, capture_diff_slices};

use crate::checkpoint::{self, Meta, MetaEntry};
use crate::doc::Document;

/// How the diff document is laid out; toggled in the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Layout {
    /// `[before]` section then `[after]` section per change.
    #[default]
    Stacked,
    /// One block per change with `-` removed and `+` added lines.
    Unified,
}

impl Layout {
    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::Stacked => Self::Unified,
            Self::Unified => Self::Stacked,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Stacked => "stacked",
            Self::Unified => "unified",
        }
    }
}

/// A comment carried from the before-version, to anchor in the rendered document.
#[derive(Debug, Clone)]
pub(crate) struct Comment {
    pub(crate) body: String,
    pub(crate) kind: String,
    /// The raw-source text the comment was attached to, so the diff can highlight that same
    /// span in the before text (as plannotator highlights a live annotation's quote).
    pub(crate) quote: String,
}

/// One changed run: the before text, the after text, and the before-version's comments on
/// it. Empty `before` means an insertion; empty `after` means a deletion.
#[derive(Debug, Clone)]
pub(crate) struct Change {
    pub(crate) before: String,
    pub(crate) after: String,
    pub(crate) comments: Vec<Comment>,
}

/// A segment of the document in order: an unchanged run (shown plain, with its comments) or
/// a changed run (shown before/after).
#[derive(Debug, Clone)]
pub(crate) enum Segment {
    /// Unchanged text (identical in both versions) and the comments on it.
    Unchanged { text: String, comments: Vec<Comment> },
    /// A changed run.
    Changed(Change),
}

/// The structured diff of two snapshots, independent of layout: the whole document as an
/// ordered list of unchanged and changed segments.
#[derive(Debug, Clone, Default)]
pub(crate) struct Diff {
    pub(crate) before_version: usize,
    pub(crate) after_version: usize,
    pub(crate) segments: Vec<Segment>,
}

impl Diff {
    /// How many segments changed.
    pub(crate) fn changed(&self) -> usize {
        self.segments.iter().filter(|s| matches!(s, Segment::Changed(_))).count()
    }
}

/// A rendered diff for one layout: the Markdown, the comments to anchor, and the word-level
/// change highlights. Comment spans are highlighted via the anchored comments (as a live
/// annotation); changed words are highlighted via `removed`/`added`.
#[derive(Debug, Clone, Default)]
pub(crate) struct DiffView {
    pub(crate) source: String,
    pub(crate) comments: Vec<DiffComment>,
    /// Byte ranges in `source` of removed words (in the before/`-` text of a change).
    pub(crate) removed: Vec<Range<usize>>,
    /// Byte ranges in `source` of added words (in the after/`+` text of a change).
    pub(crate) added: Vec<Range<usize>>,
}

/// A comment anchored into the rendered Markdown.
#[derive(Debug, Clone)]
pub(crate) struct DiffComment {
    pub(crate) range: Range<usize>,
    pub(crate) body: String,
    pub(crate) kind: String,
}

/// The two newest snapshot versions under `dir` for `base`, newest first: `(after, before)`.
fn two_newest(dir: &Path, base: &str) -> Option<(usize, usize)> {
    let prefix = format!("{base}.v");
    let mut versions: Vec<usize> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|name| {
            let rest = name.strip_prefix(&prefix)?;
            (!rest.contains('.')).then(|| rest.parse::<usize>().ok()).flatten()
        })
        .collect();
    versions.sort_unstable();
    let after = versions.pop()?;
    let before = versions.pop()?;
    Some((after, before))
}

/// Read a snapshot's source and meta for `version`.
fn read_version(dir: &Path, base: &str, version: usize) -> Result<(String, Meta)> {
    let snapshot = dir.join(format!("{base}.v{version}"));
    let source =
        std::fs::read_to_string(&snapshot).with_context(|| format!("reading {}", snapshot.display()))?;
    let meta_path = dir.join(format!("{base}.v{version}.meta"));
    let meta = match std::fs::read_to_string(&meta_path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", meta_path.display()))?,
        Err(_) => Meta::default(),
    };
    Ok((source, meta))
}

/// The comments whose recorded start falls within `range` of the before-source.
fn comments_in(meta: &Meta, range: &Range<usize>) -> Vec<Comment> {
    meta.entries
        .iter()
        .filter(|e| range.contains(&e.start))
        .map(|e: &MetaEntry| Comment {
            body: if e.body.is_empty() { "(no body)".to_owned() } else { e.body.clone() },
            kind: e.kind.clone(),
            quote: e.quote.clone(),
        })
        .collect()
}

/// Build the structured diff for `doc_path` from its two newest snapshots.
pub(crate) fn build(doc_path: &Path) -> Result<Diff> {
    let dir = checkpoint::dir_for(doc_path);
    let base =
        doc_path.file_name().map_or_else(|| "document".to_owned(), |n| n.to_string_lossy().into_owned());
    let (after_v, before_v) = two_newest(&dir, &base)
        .context("need at least two checkpoints to diff; make more with E under --checkpoint")?;

    let (before_src, before_meta) = read_version(&dir, &base, before_v)?;
    let (after_src, _after_meta) = read_version(&dir, &base, after_v)?;

    let before_doc = Document::parse(before_src.clone());
    let after_doc = Document::parse(after_src.clone());
    let before_blocks: Vec<&str> = before_doc.blocks.iter().map(|b| &before_src[b.range.clone()]).collect();
    let after_blocks: Vec<&str> = after_doc.blocks.iter().map(|b| &after_src[b.range.clone()]).collect();

    let ops = capture_diff_slices(Algorithm::Myers, &before_blocks, &after_blocks);
    let mut segments = Vec::new();
    let comments_on = |blocks: Range<usize>| {
        let mut comments = Vec::new();
        for i in blocks {
            if let Some(block) = before_doc.blocks.get(i) {
                comments.extend(comments_in(&before_meta, &block.range));
            }
        }
        comments
    };
    for op in ops {
        if let DiffOp::Equal { old_index, new_index, len } = op {
            // Unchanged: show the (shared) text and the comments on those before blocks.
            let text = join_blocks(&after_doc, &after_src, new_index..new_index + len);
            let comments = comments_on(old_index..old_index + len);
            segments.push(Segment::Unchanged { text, comments });
            continue;
        }
        let (ob, oa) = match op {
            DiffOp::Delete { old_index, old_len, .. } => (old_index..old_index + old_len, 0..0),
            DiffOp::Insert { old_index, new_index, new_len } => {
                (old_index..old_index, new_index..new_index + new_len)
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                (old_index..old_index + old_len, new_index..new_index + new_len)
            }
            DiffOp::Equal { .. } => continue,
        };
        let before = join_blocks(&before_doc, &before_src, ob.clone());
        let after = join_blocks(&after_doc, &after_src, oa);
        let comments = comments_on(ob);
        segments.push(Segment::Changed(Change { before, after, comments }));
    }
    Ok(Diff { before_version: before_v, after_version: after_v, segments })
}

/// The text of blocks `range`, joined with a blank line.
fn join_blocks(doc: &Document, src: &str, range: Range<usize>) -> String {
    range
        .filter_map(|i| doc.blocks.get(i).map(|b| src[b.range.clone()].to_owned()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render the diff into a Markdown document for `layout`: the whole document in order,
/// unchanged segments plain (with their comments highlighted in place) and changed segments
/// in before/after form. Returns the Markdown and the comments to anchor.
pub(crate) fn render(diff: &Diff, layout: Layout) -> DiffView {
    let mut view = DiffView::default();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# diff: v{} \u{2192} v{} ({})\n",
        diff.before_version,
        diff.after_version,
        layout.name()
    );
    if diff.changed() == 0 {
        out.push_str("_No block changed between these two checkpoints._\n\n");
    }
    for segment in &diff.segments {
        match segment {
            Segment::Unchanged { text, comments } => render_unchanged(&mut view, &mut out, text, comments),
            Segment::Changed(change) => {
                out.push_str("---\n\n");
                match layout {
                    Layout::Stacked => render_stacked(&mut view, &mut out, change),
                    Layout::Unified => render_unified(&mut view, &mut out, change),
                }
                out.push_str("---\n\n");
            }
        }
    }
    view.source = out;
    view
}

/// An unchanged segment: the text as-is (no diff prefix), with its comments anchored on
/// their quote spans so they highlight in place.
fn render_unchanged(view: &mut DiffView, out: &mut String, text: &str, comments: &[Comment]) {
    let spans = emit_plain(out, text);
    anchor_comments(view, text, &spans, comments);
}

/// `[before]` text then `[after]` text. Each before-comment is anchored to its quote span
/// within the before text, so the normal annotation rendering highlights what was commented
/// on; the comment body is printed beneath.
fn render_stacked(view: &mut DiffView, out: &mut String, change: &Change) {
    let (removed, added) = word_spans(&change.before, &change.after);
    out.push_str("**[before]**\n\n");
    if change.before.is_empty() {
        out.push_str("_(nothing; this block was added)_\n\n");
    } else {
        let maps = emit_prefixed(out, &change.before, "> ");
        anchor_comments(view, &change.before, &maps, &change.comments);
        record_word_spans(&mut view.removed, &maps, &removed);
    }

    out.push_str("**[after]**\n\n");
    if change.after.is_empty() {
        out.push_str("_(nothing; this block was removed)_\n\n");
    } else {
        let maps = emit_prefixed(out, &change.after, "> ");
        record_word_spans(&mut view.added, &maps, &added);
    }
}

/// One block per change: `-` lines for the before text and `+` lines for the after text,
/// with the before comments anchored to their quote spans and the changed words marked.
fn render_unified(view: &mut DiffView, out: &mut String, change: &Change) {
    let (removed, added) = word_spans(&change.before, &change.after);
    if !change.before.is_empty() {
        let maps = emit_prefixed(out, &change.before, "- ");
        anchor_comments(view, &change.before, &maps, &change.comments);
        record_word_spans(&mut view.removed, &maps, &removed);
    }
    if !change.after.is_empty() {
        let maps = emit_prefixed(out, &change.after, "+ ");
        record_word_spans(&mut view.added, &maps, &added);
    }
    out.push('\n');
}

/// Map each word span (byte range into the source text) to its range(s) in `out` via the
/// line maps, pushing the results onto `into`. A span is clipped per line so the prefix
/// offset is handled correctly.
fn record_word_spans(into: &mut Vec<Range<usize>>, maps: &[LineMap], spans: &[Range<usize>]) {
    for span in spans {
        for m in maps {
            let line_end = m.text_start + m.len;
            let s = span.start.max(m.text_start);
            let e = span.end.min(line_end);
            if s < e {
                let out_s = m.out_start + (s - m.text_start);
                let out_e = m.out_start + (e - m.text_start);
                into.push(out_s..out_e);
            }
        }
    }
}

/// Append `text` with no prefix (an unchanged segment, shown as plain Markdown). Returns
/// the line maps so comments on it anchor correctly.
fn emit_plain(out: &mut String, text: &str) -> Vec<LineMap> {
    emit_prefixed(out, text, "")
}

/// Append `text` line by line under `prefix`. Returns, per source line, the byte offset in
/// `text` where the line starts and the byte offset in `out` where its content starts, so a
/// span in `text` can be mapped into `out` (the `prefix` shifts each line).
fn emit_prefixed(out: &mut String, text: &str, prefix: &str) -> Vec<LineMap> {
    let mut maps = Vec::new();
    let mut pos = 0usize;
    for line in text.split_inclusive('\n') {
        out.push_str(prefix);
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        maps.push(LineMap { text_start: pos, out_start: out.len(), len: trimmed.len() });
        out.push_str(trimmed);
        out.push('\n');
        pos += line.len();
    }
    out.push('\n');
    maps
}

/// Where one emitted line sits: its start in the source text and in the output, and its
/// content length (without the newline).
#[derive(Debug, Clone, Copy)]
struct LineMap {
    text_start: usize,
    out_start: usize,
    len: usize,
}

/// Anchor each comment to its quote span within the just-emitted before text. `maps` came
/// from [`emit_prefixed`] for `text`. The anchor drives the normal annotation rendering
/// (highlight + rail bubble), so the comment is shown once, in the rail. A quote that is no
/// longer in the before text (its text changed) is skipped.
fn anchor_comments(view: &mut DiffView, text: &str, maps: &[LineMap], comments: &[Comment]) {
    for comment in comments {
        let quote_span = (!comment.quote.is_empty())
            .then(|| text.find(comment.quote.as_str()).map(|s| s..s + comment.quote.len()))
            .flatten();
        if let Some(range) = quote_span.and_then(|span| map_span(maps, &span)) {
            view.comments.push(DiffComment {
                range,
                body: comment.body.clone(),
                kind: comment.kind.clone(),
            });
        }
    }
}

/// Map a byte span in the source `text` to the corresponding span in `out`, using the line
/// maps. Returns the span over the first line the quote touches (a quote spanning lines is
/// anchored on its first line, which is enough to place and highlight it).
fn map_span(maps: &[LineMap], span: &Range<usize>) -> Option<Range<usize>> {
    for m in maps {
        let line_end = m.text_start + m.len;
        if span.start >= m.text_start && span.start < line_end {
            let s = m.out_start + (span.start - m.text_start);
            let e = m.out_start + (span.end.min(line_end) - m.text_start);
            return Some(s..e);
        }
    }
    None
}

/// Word-level diff of `before` vs `after`: byte ranges of removed words (into `before`) and
/// added words (into `after`). Words are maximal non-whitespace runs; whitespace between
/// them is not highlighted.
fn word_spans(before: &str, after: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let b = words(before);
    let a = words(after);
    let b_tokens: Vec<&str> = b.iter().map(|(_, w)| *w).collect();
    let a_tokens: Vec<&str> = a.iter().map(|(_, w)| *w).collect();
    let ops = capture_diff_slices(Algorithm::Myers, &b_tokens, &a_tokens);
    let mut removed = Vec::new();
    let mut added = Vec::new();
    let span = |list: &[(usize, &str)], i: usize, len: usize| {
        let start = list.get(i).map_or(0, |(o, _)| *o);
        let end = if len == 0 { start } else { list.get(i + len - 1).map_or(start, |(o, w)| o + w.len()) };
        start..end
    };
    for op in ops {
        match op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete { old_index, old_len, .. } => removed.push(span(&b, old_index, old_len)),
            DiffOp::Insert { new_index, new_len, .. } => added.push(span(&a, new_index, new_len)),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                removed.push(span(&b, old_index, old_len));
                added.push(span(&a, new_index, new_len));
            }
        }
    }
    (removed, added)
}

/// Maximal non-whitespace runs with their byte offset.
fn words(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = start.take() {
                out.push((s, &text[s..i]));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push((s, &text[s..]));
    }
    out
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, reason = "tests assert by panicking")]
mod tests {
    use super::*;
    use crate::checkpoint::Entry;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plannotator-tui-diff-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn two_versions(tag: &str, v1: &str, v1_entries: &[Entry], v2: &str) -> (PathBuf, PathBuf) {
        let root = temp_dir(tag);
        let doc = root.join("test.md");
        checkpoint::write(&doc, v1, v1_entries).expect("v1");
        checkpoint::write(&doc, v2, &[]).expect("v2");
        (root, doc)
    }

    #[test]
    fn stacked_shows_before_after_and_the_comment() {
        let v1 = "# Title\n\nalpha line\n\nbeta line\n";
        let start = v1.find("beta").expect("present");
        let entries = vec![Entry {
            id: "x".into(),
            kind: "comment".into(),
            body: "fix beta".into(),
            quote: "beta line".into(),
            range: start..start + "beta line".len(),
        }];
        let (root, doc) = two_versions("stacked", v1, &entries, "# Title\n\nalpha line\n\nBETA rewritten\n");

        let diff = build(&doc).expect("diff");
        assert_eq!((diff.before_version, diff.after_version), (1, 2));
        assert_eq!(diff.changed(), 1);
        let view = render(&diff, Layout::Stacked);
        assert!(view.source.contains("[before]"));
        assert!(view.source.contains("[after]"));
        assert!(view.source.contains("beta line"));
        assert!(view.source.contains("BETA rewritten"));
        // The whole document is shown: the unchanged heading and first paragraph appear
        // plain (no diff prefix), not just the changed block.
        assert!(view.source.contains("# Title"), "unchanged heading shown: {}", view.source);
        assert!(view.source.contains("alpha line"), "unchanged paragraph shown: {}", view.source);
        // The before comment is anchored on its quote span (shown in the rail, not inline).
        assert_eq!(view.comments.len(), 1);
        assert_eq!(view.comments[0].body, "fix beta");
        assert_eq!(&view.source[view.comments[0].range.clone()], "beta line");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn unified_marks_minus_and_plus_lines() {
        let (root, doc) =
            two_versions("unified", "# T\n\none two three\n", &[], "# T\n\none TWO three four\n");
        let diff = build(&doc).expect("diff");
        let view = render(&diff, Layout::Unified);
        assert!(view.source.contains("- one two three"), "{:?}", view.source);
        assert!(view.source.contains("+ one TWO three four"), "{:?}", view.source);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn layout_toggle_flips_between_the_two() {
        assert_eq!(Layout::Stacked.toggled(), Layout::Unified);
        assert_eq!(Layout::Unified.toggled(), Layout::Stacked);
    }

    #[test]
    fn identical_snapshots_report_no_change() {
        let src = "# Title\n\nbody\n";
        let entries = vec![Entry {
            id: "y".into(),
            kind: "comment".into(),
            body: "note".into(),
            quote: "body".into(),
            range: src.find("body").expect("present")..src.find("body").expect("present") + 4,
        }];
        let (root, doc) = two_versions("same", src, &entries, src);
        let diff = build(&doc).expect("diff");
        assert_eq!(diff.changed(), 0);
        let view = render(&diff, Layout::Stacked);
        assert!(view.source.contains("No block changed"));
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn fewer_than_two_checkpoints_is_an_error() {
        let root = temp_dir("one");
        let doc = root.join("test.md");
        checkpoint::write(&doc, "# One\n", &[]).expect("v1");
        assert!(build(&doc).is_err());
        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
