#![no_main]

//! `vfs::path` - what a path *means*, before any filesystem is consulted.
//!
//! `normalize` is what `statfs` uses to decide which mount serves a path, and
//! `components` is what resolution walks. Both are fed strings from userland
//! through `chdir`, `readlink`, `/proc` and the mount table, so the properties
//! that matter are the ones a caller relies on without checking: the answer is
//! absolute, it has no empty or dot components left in it, and running it
//! twice changes nothing.

#[path = "support.rs"]
mod support;

use libfuzzer_sys::fuzz_target;
use mpos::vfs::path;

fuzz_target!(|data: &[u8]| {
    let input = support::text(data, data.len().min(256));

    let once = path::normalize(&input);
    assert!(
        once.starts_with('/'),
        "{input:?} normalized to {once:?}, which is not absolute"
    );
    assert!(
        !once.contains("//"),
        "{input:?} normalized to {once:?}, which still has an empty component"
    );
    for component in once.split('/').filter(|c| !c.is_empty()) {
        assert_ne!(component, ".", "{input:?} normalized to {once:?}");
        assert_ne!(component, "..", "{input:?} normalized to {once:?}");
    }
    assert_eq!(
        path::normalize(&once),
        once,
        "normalizing {once:?} again moved it"
    );

    // Splitting is only defined for an absolute path; anything else is
    // `Invalid`. The components of one that survived are never empty, and
    // putting them back with a single slash between them has to normalize to
    // the same thing the original did - otherwise resolution and `statfs`
    // disagree about which path a caller named.
    if let Ok(components) = path::components(&input) {
        let parts: Vec<&str> = components.collect();
        for component in &parts {
            assert!(
                !component.is_empty(),
                "{input:?} yielded an empty component"
            );
        }
        let rejoined = format!("/{}", parts.join("/"));
        assert_eq!(
            path::normalize(&rejoined),
            once,
            "{input:?} and its own components do not mean the same path"
        );
    }
});
