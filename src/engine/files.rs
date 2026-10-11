//! The `filepaths` and `folders` templates: suggestions read from the file system.

use std::path::{Path, PathBuf};

use super::spec::Template;
use super::{Collector, starts_with_ignore_case};

/// Directory entries examined per request, so a huge directory cannot stall the engine.
const MAX_SCAN: usize = 5000;

/// The entries of a directory that match what has been typed of a path.
pub(super) struct Listing {
    /// The part of the path before its last component, kept exactly as typed.
    pub typed_dir: String,
    /// Names with whether each is a directory, sorted without regard to case.
    pub entries: Vec<(String, bool)>,
}

/// Lists the files `query`, a partly typed path, could go on to name. Only the last component
/// is being completed.
pub(super) fn list(query: &str, template: Template, cwd: &Path) -> Listing {
    let (typed_dir, partial) = match query.rfind('/') {
        Some(slash) => query.split_at(slash + 1),
        None => ("", query),
    };
    let show_hidden = partial.starts_with('.');
    let mut entries: Vec<(String, bool)> = resolve(cwd, typed_dir)
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .take(MAX_SCAN)
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            if (name.starts_with('.') && !show_hidden) || !starts_with_ignore_case(&name, partial) {
                return None;
            }
            let kind = entry.file_type().ok()?;
            let is_dir = kind.is_dir() || (kind.is_symlink() && entry.path().is_dir());
            (is_dir || template == Template::Filepaths).then_some((name, is_dir))
        })
        .collect();
    entries.sort_by_cached_key(|(name, _)| name.to_lowercase());
    Listing {
        typed_dir: typed_dir.to_owned(),
        entries,
    }
}

pub(super) fn suggest(out: &mut Collector, template: Template, cwd: &Path) {
    let listing = list(&out.query, template, cwd);
    for (name, is_dir) in &listing.entries {
        out.file(&listing.typed_dir, name, *is_dir);
    }
}

fn resolve(cwd: &Path, typed_dir: &str) -> Option<PathBuf> {
    if let Some(rest) = typed_dir.strip_prefix("~/") {
        return Some(PathBuf::from(std::env::var_os("HOME")?).join(rest));
    }
    Some(cwd.join(typed_dir))
}
