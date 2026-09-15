//! Binary formats: the one piece of interpretation the kernel does for a
//! program.
//!
//! Everything else about running a file is a loader's business - read the
//! header, lay out the segments, jump to the entry. The exception is a file
//! that opens with `#!`: that file is not a program at all, it is the *name* of
//! the program that runs it, plus the argument to hand it. No loader can
//! resolve that, because resolving it means reading another file, and the
//! kernel is the thing with the authority to do that.
//!
//! So this module is deliberately small and pure: the rule for reading the
//! first line of a file, with no filesystem anywhere near it. The caller -
//! `execve` - is the part that can follow the name it produces, and it is the
//! part that has to answer for a chain that never ends.

use alloc::string::String;

/// How much of the first line is read for an interpreter. Linux reads the same
/// window: the line is a fixed-size field at the top of the file, not a string
/// to be scanned to its end.
pub const SHEBANG_LINE_MAX: usize = 256;

/// How many `#!` hops to follow before calling it a loop. Linux caps the chain
/// the same way and for the same reason: an interpreter may itself be a script,
/// but not forever.
pub const SHEBANG_HOPS_MAX: usize = 4;

/// The interpreter line at the top of `bytes`, if there is one: the path after
/// `#!`, and the single optional argument that may follow it.
///
/// Everything after the interpreter's name up to the newline is **one**
/// argument, not a command line to be split again - the part that surprises
/// anyone expecting a shell's word splitting here. `#!/bin/sh -e -x` names the
/// single argument `-e -x`, which is why a script cannot pass two options to
/// its interpreter from the shebang line.
///
/// `None` for a file that does not open with `#!`, and for one whose `#!` is
/// followed by nothing but space: neither names anything to run.
pub fn shebang(bytes: &[u8]) -> Option<(String, Option<String>)> {
    if !bytes.starts_with(b"#!") {
        return None;
    }
    let end = bytes.len().min(SHEBANG_LINE_MAX);
    let line = &bytes[2..end];
    let line = match line.iter().position(|&b| b == b'\n') {
        Some(at) => &line[..at],
        None => line,
    };
    let text = core::str::from_utf8(line).ok()?;
    let text = text.trim_start_matches([' ', '\t']);
    if text.is_empty() {
        return None;
    }
    let (interp, rest) = match text.find([' ', '\t']) {
        Some(at) => (&text[..at], &text[at + 1..]),
        None => (text, ""),
    };
    let argument = rest.trim_end_matches([' ', '\t', '\r']);
    Some((
        String::from(interp),
        if argument.is_empty() {
            None
        } else {
            Some(String::from(argument))
        },
    ))
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::{SHEBANG_LINE_MAX, shebang};

    fn parts(line: &str) -> Option<(String, Option<String>)> {
        shebang(line.as_bytes())
    }

    #[test]
    fn an_interpreter_line_is_read_whole() {
        assert_eq!(parts("#!/bin/sh"), Some((String::from("/bin/sh"), None)));
        assert_eq!(
            parts("#!/bin/sh\necho hi\n"),
            Some((String::from("/bin/sh"), None))
        );
        // Whatever follows the name on the line is *one* argument, not a
        // command line.
        assert_eq!(
            parts("#!/bin/sh -e -x\n"),
            Some((String::from("/bin/sh"), Some(String::from("-e -x"))))
        );
        // Space between `#!` and the name is allowed, and trailing space is
        // not an empty argument.
        assert_eq!(
            parts("#!  /usr/bin/env  \t"),
            Some((String::from("/usr/bin/env"), None))
        );
        assert_eq!(
            parts("#!/usr/bin/env python3\n"),
            Some((String::from("/usr/bin/env"), Some(String::from("python3"))))
        );
    }

    #[test]
    fn what_is_not_an_interpreter_line_is_not_one() {
        // Not a script at all: an ELF header, a plain text file, an empty one.
        assert_eq!(shebang(b"\x7fELF"), None);
        assert_eq!(parts("just text\n"), None);
        assert_eq!(shebang(b""), None);
        // `#!` with nothing after it names nothing to run.
        assert_eq!(shebang(b"#!"), None);
        assert_eq!(shebang(b"#!   \n"), None);
        // A file opens a chain only if `#!` is the very first thing in it.
        assert_eq!(shebang(b" #!/bin/sh\n"), None);
        // And a line that is not valid UTF-8 names nothing this kernel can
        // read, so it is not a chain either.
        assert_eq!(shebang(b"#!/bin/\xffsh\n"), None);
    }

    #[test]
    fn the_line_is_a_bounded_field_not_a_scan_to_the_end() {
        // A line longer than the window is cut at the window, as Linux cuts it:
        // the name is at the beginning, which is where the reading starts.
        let long = alloc::format!("#!/bin/sh {}", "x".repeat(SHEBANG_LINE_MAX * 2));
        match shebang(long.as_bytes()) {
            Some((interp, argument)) => {
                assert_eq!(interp, "/bin/sh");
                let argument = argument.expect("the rest of the line is the argument");
                assert!(argument.len() < SHEBANG_LINE_MAX, "cut at the window");
            }
            None => panic!("a long line still names its interpreter"),
        }
    }
}
