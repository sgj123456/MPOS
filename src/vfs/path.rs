//! Absolute path splitting.
//!
//! Resolution walks components one at a time so `..` can pop against the real
//! tree and mount crossings happen at the right instant, which lexical
//! normalization up front would get wrong once symlinks exist. `.` and `..`
//! are therefore handed through as components, not resolved here.

use super::VfsError;
use super::VfsResult;

/// The components of an absolute path, empty runs of `/` collapsed.
pub struct Components<'a> {
    rest: &'a str,
}

impl<'a> Iterator for Components<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        self.rest = self.rest.trim_start_matches('/');
        if self.rest.is_empty() {
            return None;
        }
        match self.rest.find('/') {
            Some(i) => {
                let (comp, rest) = self.rest.split_at(i);
                self.rest = rest;
                Some(comp)
            }
            None => {
                let comp = self.rest;
                self.rest = "";
                Some(comp)
            }
        }
    }
}

/// Split an absolute path. Relative paths are [`VfsError::Invalid`] until
/// processes (and with them, cwd) exist.
/// The canonical spelling of `path`: absolute, with `.` dropped, `..` folded
/// into the components before it, and repeated slashes collapsed.
///
/// This is not resolution - nothing here touches a filesystem. It is what a
/// path *means*, and the caller that needs it most is `statfs`: the filesystem
/// serving a path is found by matching mountpoint prefixes, and a mountpoint
/// is never spelled `.`.
///
/// ```
/// use mpos::vfs::path::normalize;
///
/// assert_eq!(normalize("/etc/../etc/passwd"), "/etc/passwd"); // .. folds back
/// assert_eq!(normalize("//a/./b//"), "/a/b"); // slashes collapse, . drops
/// assert_eq!(normalize(""), "/"); // the empty path means the root
/// assert_eq!(normalize("/.."), "/"); // nothing escapes above the root
/// ```
pub fn normalize(path: &str) -> alloc::string::String {
    let mut parts: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => continue,
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return alloc::string::String::from("/");
    }
    let mut out = alloc::string::String::new();
    for part in parts {
        out.push('/');
        out.push_str(part);
    }
    out
}

pub fn components(path: &str) -> VfsResult<Components<'_>> {
    if !path.starts_with('/') {
        return Err(VfsError::Invalid);
    }
    Ok(Components { rest: path })
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{components, normalize};
    use crate::vfs::VfsError;

    fn parts(path: &str) -> Vec<&str> {
        components(path).unwrap().collect()
    }

    #[test]
    fn absolute_paths_split_into_components() {
        assert_eq!(parts("/"), Vec::<&str>::new());
        assert_eq!(parts("/a"), vec!["a"]);
        assert_eq!(parts("/a/b/c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn repeated_and_trailing_slashes_collapse() {
        assert_eq!(parts("//a///b//"), vec!["a", "b"]);
    }

    #[test]
    fn dots_pass_through_for_resolution_to_interpret() {
        assert_eq!(parts("/a/./b/../c"), vec!["a", ".", "b", "..", "c"]);
    }

    /// `normalize` is not resolution - it never touches a filesystem - but it
    /// is what `statfs` matches a mountpoint against, so two spellings of one
    /// path have to come out as one string. `.` is dropped, `..` folds into
    /// the name before it, and runs of slashes collapse.
    #[test]
    fn normalize_drops_dots_and_collapses_repeated_slashes() {
        assert_eq!(normalize("/a/b"), "/a/b");
        assert_eq!(normalize("/a//b//c"), "/a/b/c");
        assert_eq!(normalize("/a/./b"), "/a/b");
        assert_eq!(normalize("/a/x/../b"), "/a/b");
        assert_eq!(normalize("/a/b/"), "/a/b");
        assert_eq!(normalize("//"), "/");
    }

    /// There is nothing above the root to climb into, and a path that says
    /// otherwise is the ordinary "too many `..`" a userland tool produces when
    /// it builds a path by hand rather than by walking.
    #[test]
    fn normalize_cannot_climb_above_the_root() {
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("/../.."), "/");
        assert_eq!(normalize("/a/../.."), "/");
    }

    /// Every caller hands `normalize` an absolute path - a mountpoint is never
    /// spelled `.` - and a relative one is read as if it hung off the root,
    /// which is what makes the prefix match below behave. It is a spelling
    /// rule, not a resolution: a `cwd` is the resolver's business, not this
    /// function's.
    #[test]
    fn normalize_spells_a_relative_path_from_the_root() {
        assert_eq!(normalize("a/b"), "/a/b");
        assert_eq!(normalize(""), "/");
    }

    #[test]
    fn relative_paths_are_rejected_while_there_is_no_cwd() {
        assert_eq!(components("a/b").err(), Some(VfsError::Invalid));
        assert_eq!(components("").err(), Some(VfsError::Invalid));
        assert_eq!(components("./a").err(), Some(VfsError::Invalid));
    }
}
