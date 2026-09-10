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

    use super::components;
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

    #[test]
    fn relative_paths_are_rejected_while_there_is_no_cwd() {
        assert_eq!(components("a/b").err(), Some(VfsError::Invalid));
        assert_eq!(components("").err(), Some(VfsError::Invalid));
        assert_eq!(components("./a").err(), Some(VfsError::Invalid));
    }
}
