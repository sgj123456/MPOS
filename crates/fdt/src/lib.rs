#![no_std]

//! Flattened device trees: the machine, describing itself, as bytes.
//!
//! A device tree is how a machine that has no PCI and no ACPI says what it has:
//! an array of nodes, each with properties, all of it a flat blob of
//! big-endian words. On `virt` the firmware passes one to the loader, and from
//! stage 5 the loader hands it to the kernel - so the addresses the kernel
//! probes stop being constants it was compiled with and become answers it read
//! (`PLAN.md` §9.1).
//!
//! Why this is its own crate: nothing in it can fail at boot for a reason a
//! host test cannot reproduce. Every question it answers - is this blob a
//! device tree, where do this node's properties end, how wide is this `reg`
//! written - is arithmetic on a `&[u8]`, and arithmetic is worth a test. The
//! machine half (which compatible string means the serial port) stays in
//! `arch`, where naming a machine is legal.
//!
//! What it deliberately does not do: allocate (the whole reader is a walk over
//! the caller's bytes), resolve anything (a `phandle` comes back as the number
//! it is, because what points at what is the caller's business), or interpret
//! what a property *means* - `interrupts` comes back as cells, and that the
//! first cell of a triple says whether it is a shared or a per-CPU line is the
//! interrupt controller's encoding, not the format's.
//!
//! Two places the format is more permissive than this reader, both named
//! because both are ways a firmware could otherwise make the kernel read past
//! its own buffer: the block offsets in the header are checked against the
//! buffer that *arrived* rather than against the size the header claims
//! (`totalsize` is a claim, and QEMU's is a megabyte around an eight-kilobyte
//! tree), and every property's name offset is checked against the strings block
//! while its node is read - which is what lets the accessors on a node answer
//! `Option` instead of a result nobody would handle.

extern crate alloc;

/// The four-byte token that opens a node, followed by its name.
const BEGIN_NODE: u32 = 1;
/// The token that closes one.
const END_NODE: u32 = 2;
/// A property: its length, its name's offset into the strings block, its value.
const PROP: u32 = 3;
/// Padding, and the reason every token can be read as a word.
const NOP: u32 = 4;
/// The last token in the structure block.
const END: u32 = 9;

/// `0xd00dfeed`, at offset 0 of every blob. Big-endian, like everything here.
const MAGIC: u32 = 0xd00d_feed;

/// The oldest version whose structure block this reader walks. 16 is where
/// `totalsize` and the mandatory `FDT_END` were defined; 17 is what QEMU writes.
/// An older blob is not a tree this kernel can read, and pretending otherwise
/// would mean guessing at a layout nothing has written for twenty years.
const MIN_VERSION: u32 = 16;

/// The largest blob this reader will look at.
///
/// A tree is a few kilobytes - this machine's is 7160 bytes of structure and
/// 569 of strings - so this is not a size limit but a liveness one: a firmware
/// handing over a nonsense `totalsize` is refused before anything walks a byte
/// of it. `boot`'s copy of the tree is bounded by the same number, so a tree
/// that reached the kernel is one this reader will read.
pub const MAX_BYTES: usize = 4 * 1024 * 1024;

/// How deep a tree may nest before the reader gives up.
///
/// The bound exists because the walk keeps one frame of inherited cell counts
/// per level in a fixed array, and a corrupt blob must not be able to drive it
/// past that. This machine's tree nests three levels; Linux's limit is 64.
const MAX_DEPTH: usize = 32;

/// The header's size. The two blocks follow it, at offsets the header gives;
/// this is what the tests write.
pub const HEADER_SIZE: usize = 40;

/// Why a blob is not a tree this reader will walk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The first word is not `0xd00dfeed`, so it is not a device tree at all.
    Magic(u32),
    /// Larger than [`MAX_BYTES`].
    TooLarge(usize),
    /// Shorter than the header, or a header field naming bytes the buffer does
    /// not have. The string names the field.
    Truncated(&'static str),
    /// A version older than [`MIN_VERSION`].
    Version(u32),
    /// More than [`MAX_DEPTH`] nodes deep without an end.
    TooDeep,
    /// A structure block that ends without `FDT_END`, or an `FDT_END_NODE`
    /// with no node open.
    Unbalanced,
    /// A word that is not one of the five tokens.
    Token(u32),
    /// A property name offset outside the strings block, or a name in it that
    /// runs to the end without a terminator.
    NameOffset(u32),
    /// A node name that is not terminated inside the structure block, or is not
    /// UTF-8.
    NodeName,
}

/// A device tree, borrowed from the bytes it arrived in.
///
/// [`Copy`], so a [`Node`] can hold one by value and hand it back to a caller
/// with the blob's own lifetime rather than the walk's.
#[derive(Clone, Copy, Debug)]
pub struct Fdt<'a> {
    /// The structure block: the nodes and their properties.
    structure: &'a [u8],
    /// The strings block: every property name in the tree, once each.
    strings: &'a [u8],
    /// What the header claims the whole blob is.
    total_size: usize,
}

impl<'a> Fdt<'a> {
    /// Read the header, and check that the two blocks it names are inside the
    /// bytes that arrived.
    pub fn new(bytes: &'a [u8]) -> Result<Fdt<'a>, Error> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::TooLarge(bytes.len()));
        }
        if bytes.len() < HEADER_SIZE {
            return Err(Error::Truncated("header"));
        }
        let magic = word_at(bytes, 0);
        if magic != MAGIC {
            return Err(Error::Magic(magic));
        }
        // Against the buffer, not against `totalsize`: a header may claim more
        // bytes than it has, and walking where it points is the one mistake
        // this reader exists to not make.
        let structure = block(
            bytes,
            word_at(bytes, 8) as usize,
            word_at(bytes, 36) as usize,
        )?;
        let strings = block(
            bytes,
            word_at(bytes, 12) as usize,
            word_at(bytes, 32) as usize,
        )?;
        let version = word_at(bytes, 20);
        if version < MIN_VERSION {
            return Err(Error::Version(version));
        }
        Ok(Fdt {
            structure,
            strings,
            total_size: word_at(bytes, 4) as usize,
        })
    }

    /// The size the header claims for the whole blob.
    ///
    /// Not the length of the buffer it arrived in, and the two differ in
    /// practice: QEMU writes the tree and declares the maximum it reserved, so
    /// its `virt` blob claims a megabyte and holds eight kilobytes.
    pub fn total_size(&self) -> usize {
        self.total_size
    }

    /// The first node, in the order the tree lists them, that `matches` accepts.
    pub fn find(&self, matches: impl Fn(&Node<'a>) -> bool) -> Result<Option<Node<'a>>, Error> {
        let mut found = None;
        self.each(matches, |node| {
            found = Some(node);
            false
        })?;
        Ok(found)
    }

    /// [`find`](Fdt::find) for the common case: a node that can be driven by
    /// `compatible`.
    pub fn find_compatible(&self, compatible: &str) -> Result<Option<Node<'a>>, Error> {
        self.find(|node| node.is_compatible(compatible))
    }

    /// Call `f` on every node `matches` accepts, depth-first. Stops as soon as
    /// `f` answers `false`.
    ///
    /// A node is handed over with its own properties already in hand, which is
    /// why the walk bounds the property records and a caller reads them again
    /// when it asks - rather than buffering a node nobody may ask about. A tree
    /// is small and read once.
    pub fn each(
        &self,
        matches: impl Fn(&Node<'a>) -> bool,
        mut f: impl FnMut(Node<'a>) -> bool,
    ) -> Result<(), Error> {
        let mut at = 0usize;
        let mut depth = 0usize;
        // The (#address-cells, #size-cells) in force, one frame per open node.
        // A node's own declaration governs its *children* - that is what makes
        // a parent's `reg` readable in a parent's widths - so a frame holds the
        // value its children inherit, and is restored when the node closes. The
        // root has no parent; the spec's defaults are 2 and 1.
        let mut cells = [(2u32, 1u32); MAX_DEPTH];
        let mut current = (2u32, 1u32);

        loop {
            let token = self.word_in(at)?;
            at += 4;
            match token {
                NOP => {}
                END => {
                    return if depth == 0 {
                        Ok(())
                    } else {
                        Err(Error::Unbalanced)
                    };
                }
                END_NODE => {
                    if depth == 0 {
                        return Err(Error::Unbalanced);
                    }
                    depth -= 1;
                    current = cells[depth];
                }
                BEGIN_NODE => {
                    if depth == MAX_DEPTH {
                        return Err(Error::TooDeep);
                    }
                    let (name, after_name) = self.node_name(at)?;
                    let (props, after_props) = self.properties(after_name)?;
                    at = after_props;
                    let node = Node {
                        fdt: *self,
                        name,
                        props,
                        address_cells: current.0,
                        size_cells: current.1,
                    };
                    cells[depth] = current;
                    // One at a time, because the format inherits them one at a
                    // time: a node that gives a size and no address length
                    // takes its parent's address length.
                    current = (
                        node.u32_property("#address-cells").unwrap_or(current.0),
                        node.u32_property("#size-cells").unwrap_or(current.1),
                    );
                    depth += 1;
                    if matches(&node) && !f(node) {
                        return Ok(());
                    }
                }
                other => return Err(Error::Token(other)),
            }
        }
    }

    /// A four-byte word of the structure block.
    fn word_in(&self, at: usize) -> Result<u32, Error> {
        self.structure
            .get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or(Error::Truncated("structure block"))
    }

    /// A node's name, and the offset just past its padding.
    fn node_name(&self, at: usize) -> Result<(&'a str, usize), Error> {
        let rest = self
            .structure
            .get(at..)
            .ok_or(Error::Truncated("node name"))?;
        let len = rest.iter().position(|b| *b == 0).ok_or(Error::NodeName)?;
        let name = core::str::from_utf8(&rest[..len]).map_err(|_| Error::NodeName)?;
        Ok((name, pad(at + len + 1)))
    }

    /// The property records that follow a node's name, as one run of bytes, and
    /// the offset of the first token that is not one.
    ///
    /// This is where every name offset in the node is checked, so that asking a
    /// node for a property afterwards cannot fail.
    fn properties(&self, mut at: usize) -> Result<(&'a [u8], usize), Error> {
        let start = at;
        while self.word_in(at)? == PROP {
            let len = self.word_in(at + 4)? as usize;
            self.string(word_at(self.structure, at + 8))?;
            at = at
                .checked_add(12 + pad(len))
                .ok_or(Error::Truncated("property"))?;
            if at > self.structure.len() {
                return Err(Error::Truncated("property value"));
            }
        }
        Ok((&self.structure[start..at], at))
    }

    /// A property name, from its offset into the strings block.
    fn string(&self, offset: u32) -> Result<&'a str, Error> {
        let rest = self
            .strings
            .get(offset as usize..)
            .ok_or(Error::NameOffset(offset))?;
        let len = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or(Error::NameOffset(offset))?;
        core::str::from_utf8(&rest[..len]).map_err(|_| Error::NameOffset(offset))
    }
}

/// One node of a tree, with its own properties in hand.
///
/// Its methods are the questions a machine asks of a node: what are you called,
/// what can drive you, where is your register window, and what does a property
/// of yours hold.
#[derive(Clone, Copy, Debug)]
pub struct Node<'a> {
    fdt: Fdt<'a>,
    name: &'a str,
    /// This node's own property records, back to back.
    props: &'a [u8],
    /// The address length this node's `reg` is written in: its *parent's*
    /// declaration, not its own.
    address_cells: u32,
    /// The size length, likewise.
    size_cells: u32,
}

impl<'a> Node<'a> {
    /// The node's own name, as the tree spells it - `pl011@9000000`. The root
    /// node's name is the empty string, which is what the format writes for it.
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// The raw bytes of `name`, or `None` when this node has no such property.
    fn value(&self, name: &str) -> Option<&'a [u8]> {
        let mut at = 0usize;
        while let Some(rest) = self.props.get(at..) {
            if rest.len() < 12 {
                return None;
            }
            let len = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
            let name_offset = u32::from_be_bytes([rest[8], rest[9], rest[10], rest[11]]);
            let start = at + 12;
            let value = self.props.get(start..start.checked_add(len)?)?;
            if self.fdt.string(name_offset) == Ok(name) {
                return Some(value);
            }
            at = pad(start + len);
        }
        None
    }

    /// A property holding one NUL-terminated string.
    ///
    /// `compatible` and `device_type` are the two this machine reads; both are
    /// single strings, where a node's list of bindings is a `compatible` that
    /// this answers only its first entry of - [`is_compatible`] is the one that
    /// reads the whole list.
    ///
    /// [`is_compatible`]: Node::is_compatible
    pub fn string_property(&self, name: &str) -> Option<&'a str> {
        let bytes = self.value(name)?;
        let len = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        core::str::from_utf8(&bytes[..len]).ok()
    }

    /// A property holding one big-endian word.
    pub fn u32_property(&self, name: &str) -> Option<u32> {
        let bytes = self.value(name)?;
        (bytes.len() >= 4).then(|| word_at(bytes, 0))
    }

    /// Every word of a property - `interrupts`, or anything else a caller
    /// decodes for itself.
    pub fn cells(&self, name: &str) -> Cells<'a> {
        Cells {
            bytes: self.value(name).unwrap_or(&[]),
            at: 0,
        }
    }

    /// Whether this node's `compatible` list contains `compatible`.
    ///
    /// The list is what the format writes for a node more than one binding can
    /// drive: most specific first, and the entry a driver matches on is often
    /// not the first - this machine's serial port is `arm,pl011` and then
    /// `arm,primecell`.
    pub fn is_compatible(&self, compatible: &str) -> bool {
        let Some(list) = self.value("compatible") else {
            return false;
        };
        list.split(|b| *b == 0)
            .any(|entry| core::str::from_utf8(entry) == Ok(compatible))
    }

    /// This node's register windows, as `(address, size)` pairs.
    ///
    /// A window has a size as well as an address, and a node may have more than
    /// one: this machine's interrupt controller is a distributor at one address
    /// and a CPU interface at another, written as one `reg` of two entries.
    pub fn reg(&self) -> Reg<'a> {
        Reg {
            cells: self.cells("reg"),
            address_cells: self.address_cells as usize,
            size_cells: self.size_cells as usize,
        }
    }
}

/// The words of a property, big-endian, one at a time.
#[derive(Clone, Copy)]
pub struct Cells<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cells<'_> {
    /// How many whole words are left, which is what a reader needs before it
    /// takes a window's worth: a short `reg` should end the iteration rather
    /// than leave a caller mid-entry.
    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at) / 4
    }
}

impl Iterator for Cells<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let word = self.bytes.get(self.at..self.at + 4)?;
        self.at += 4;
        Some(u32::from_be_bytes([word[0], word[1], word[2], word[3]]))
    }
}

/// A node's register windows.
pub struct Reg<'a> {
    cells: Cells<'a>,
    address_cells: usize,
    size_cells: usize,
}

impl Iterator for Reg<'_> {
    type Item = (u64, u64);

    /// The next window, or `None` when the property is exhausted - including
    /// when fewer words are left than one window takes, which is a `reg` whose
    /// last entry was cut short.
    fn next(&mut self) -> Option<(u64, u64)> {
        if self.cells.remaining() < self.address_cells + self.size_cells {
            return None;
        }
        // Folded from the high word down: 64 bits come out of a two-cell
        // address, one word out of a one-cell address, and a three-cell address
        // - which the format allows and no 64-bit machine writes - keeps the
        // low 64 of it rather than refusing.
        let mut address = 0u64;
        for _ in 0..self.address_cells {
            address = (address << 32) | u64::from(self.cells.next()?);
        }
        let mut size = 0u64;
        for _ in 0..self.size_cells {
            size = (size << 32) | u64::from(self.cells.next()?);
        }
        Some((address, size))
    }
}

/// A big-endian word at `at`, or zero when there is nothing there.
///
/// Two callers want the zero: the header reads, whose offsets are checked by
/// the bounds of the slice itself, and a property name offset the walk has
/// already validated. Every other read goes through `Fdt::word_in`, which
/// answers a result.
fn word_at(bytes: &[u8], at: usize) -> u32 {
    match bytes.get(at..at + 4) {
        Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

/// `at` rounded up to a four-byte boundary, which is where every token after a
/// name or a property value starts.
fn pad(at: usize) -> usize {
    at.next_multiple_of(4)
}

/// One of the header's two block offsets, as a slice of `bytes`.
fn block(bytes: &[u8], at: usize, len: usize) -> Result<&[u8], Error> {
    let end = at.checked_add(len).ok_or(Error::Truncated("block"))?;
    bytes.get(at..end).ok_or(Error::Truncated("block"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A device tree built in memory, so a test can write exactly the bytes it
    /// means. The format shares one string table between every property name;
    /// this builder appends a name per property instead, which is legal and
    /// much easier to read.
    struct Tree {
        structure: Vec<u8>,
        strings: Vec<u8>,
    }

    impl Tree {
        fn new() -> Tree {
            Tree {
                structure: Vec::new(),
                strings: Vec::new(),
            }
        }

        fn begin(mut self, name: &str) -> Tree {
            self.push(BEGIN_NODE);
            self.structure.extend_from_slice(name.as_bytes());
            self.structure.push(0);
            self.align();
            self
        }

        fn end(mut self) -> Tree {
            self.push(END_NODE);
            self
        }

        /// The end of the structure block. Every tree needs one; a tree without
        /// it is the `Truncated` case, not the well-formed one.
        fn finish(mut self) -> Tree {
            self.push(END);
            self
        }

        fn push(&mut self, value: u32) {
            self.structure.extend_from_slice(&value.to_be_bytes());
        }

        fn align(&mut self) {
            while !self.structure.len().is_multiple_of(4) {
                self.structure.push(0);
            }
        }

        fn prop(mut self, name: &str, value: &[u8]) -> Tree {
            let offset = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            self.push(PROP);
            self.push(value.len() as u32);
            self.push(offset);
            self.structure.extend_from_slice(value);
            self.align();
            self
        }

        /// One string, as `compatible` or `device_type` is written.
        fn text(self, name: &str, value: &str) -> Tree {
            let mut bytes = value.as_bytes().to_vec();
            bytes.push(0);
            self.prop(name, &bytes)
        }

        /// A NUL-separated list of strings, as a `compatible` list is written.
        fn list(self, name: &str, values: &[&str]) -> Tree {
            let mut bytes = Vec::new();
            for value in values {
                bytes.extend_from_slice(value.as_bytes());
                bytes.push(0);
            }
            self.prop(name, &bytes)
        }

        fn words(self, name: &str, values: &[u32]) -> Tree {
            let mut bytes = Vec::new();
            for value in values {
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            self.prop(name, &bytes)
        }

        /// The blob: a header, the structure block, the strings block.
        fn blob(&self) -> Vec<u8> {
            let strings_at = HEADER_SIZE + self.structure.len();
            let mut out = Vec::new();
            for field in [
                MAGIC,
                (strings_at + self.strings.len()) as u32, // totalsize
                HEADER_SIZE as u32,                       // off_dt_struct
                strings_at as u32,                        // off_dt_strings
                0,                                        // off_mem_rsvmap
                17,                                       // version
                16,                                       // last_comp_version
                0,                                        // boot_cpuid_phys
                self.strings.len() as u32,                // size_dt_strings
                self.structure.len() as u32,              // size_dt_struct
            ] {
                out.extend_from_slice(&field.to_be_bytes());
            }
            out.extend_from_slice(&self.structure);
            out.extend_from_slice(&self.strings);
            out
        }
    }

    /// The shape of the tree the `virt` machine hands over, holding the parts
    /// this reader is asked about: the same compatible strings, the same
    /// two-cell addresses, the same shapes of `reg`. Written out rather than
    /// copied from a dump, because a dump is a megabyte of padding around eight
    /// kilobytes of meaning, and the meaning is the part worth reading.
    fn virt() -> Tree {
        Tree::new()
            .begin("")
            .words("#address-cells", &[2])
            .words("#size-cells", &[2])
            .list("compatible", &["linux,dummy-virt"])
            .begin("memory@40000000")
            .text("device_type", "memory")
            .words("reg", &[0, 0x4000_0000, 0, 0x1000_0000])
            .end()
            .begin("pl011@9000000")
            .words("interrupts", &[0, 1, 4])
            .words("reg", &[0, 0x0900_0000, 0, 0x1000])
            .list("compatible", &["arm,pl011", "arm,primecell"])
            .end()
            .begin("intc@8000000")
            .words(
                "reg",
                &[0, 0x0800_0000, 0, 0x1_0000, 0, 0x0801_0000, 0, 0x1_0000],
            )
            .list("compatible", &["arm,cortex-a15-gic"])
            .words("#interrupt-cells", &[3])
            .begin("v2m@8020000")
            .words("reg", &[0, 0x0802_0000, 0, 0x1000])
            .end()
            .end()
            .begin("virtio_mmio@a000000")
            .words("reg", &[0, 0x0a00_0000, 0, 0x200])
            .list("compatible", &["virtio,mmio"])
            .end()
            .begin("virtio_mmio@a000200")
            .words("reg", &[0, 0x0a00_0200, 0, 0x200])
            .list("compatible", &["virtio,mmio"])
            .end()
            .begin("timer")
            .words(
                "interrupts",
                &[1, 13, 0x104, 1, 14, 0x104, 1, 11, 0x104, 1, 10, 0x104],
            )
            .list("compatible", &["arm,armv8-timer"])
            .end()
            .end()
            .finish()
    }

    #[test]
    fn the_tree_reads_back_the_way_the_machine_wrote_it() {
        let blob = virt().blob();
        let fdt = Fdt::new(&blob).expect("the tree is well formed");
        assert_eq!(fdt.total_size(), blob.len());

        // A node is found by any string in its compatible list, not only the
        // first - which is what a driver matching `arm,pl011` depends on.
        let serial = fdt
            .find_compatible("arm,pl011")
            .unwrap()
            .expect("a serial port");
        assert_eq!(serial.name(), "pl011@9000000");
        assert!(serial.is_compatible("arm,primecell"));
        assert!(serial.is_compatible("arm,pl011"));
        assert!(!serial.is_compatible("arm,pl011x"));
        // Its address is two cells of its parent's declaration, its size the
        // two that follow.
        assert_eq!(
            serial.reg().collect::<Vec<_>>(),
            vec![(0x0900_0000, 0x1000)]
        );
        assert_eq!(serial.u32_property("#address-cells"), None);
        assert_eq!(
            serial.cells("interrupts").collect::<Vec<_>>(),
            vec![0, 1, 4]
        );

        // The interrupt controller is one `reg` of two windows.
        let gic = fdt
            .find_compatible("arm,cortex-a15-gic")
            .unwrap()
            .expect("a GIC");
        assert_eq!(
            gic.reg().collect::<Vec<_>>(),
            vec![(0x0800_0000, 0x1_0000), (0x0801_0000, 0x1_0000)]
        );
        assert_eq!(gic.u32_property("#interrupt-cells"), Some(3));

        // A node with no `compatible` at all is reached by what it declares
        // instead, which is how a memory node is written.
        let memory = fdt
            .find(|node| node.string_property("device_type") == Some("memory"))
            .unwrap()
            .expect("a memory node");
        assert_eq!(memory.name(), "memory@40000000");
        assert_eq!(
            memory.reg().collect::<Vec<_>>(),
            vec![(0x4000_0000, 0x1000_0000)]
        );

        // The timer's four interrupt triples come back as twelve cells, which
        // is what the caller decodes.
        let timer = fdt
            .find_compatible("arm,armv8-timer")
            .unwrap()
            .expect("a timer");
        let interrupts = timer.cells("interrupts").collect::<Vec<_>>();
        assert_eq!(interrupts.len(), 12);
        assert_eq!(&interrupts[3..6], &[1, 14, 0x104]);

        // Every node a match asks for, in the order the tree lists them.
        let mut names = Vec::new();
        fdt.each(
            |node| node.is_compatible("virtio,mmio"),
            |node| {
                names.push(node.name());
                true
            },
        )
        .unwrap();
        assert_eq!(names, vec!["virtio_mmio@a000000", "virtio_mmio@a000200"]);

        // A child of a child is still walked: the V2M under the GIC.
        let v2m = fdt
            .find(|node| node.name() == "v2m@8020000")
            .unwrap()
            .expect("the V2M");
        assert_eq!(v2m.reg().collect::<Vec<_>>(), vec![(0x0802_0000, 0x1000)]);

        assert!(fdt.find_compatible("arm,pl022").unwrap().is_none());
    }

    #[test]
    fn the_walk_stops_when_the_caller_says_so() {
        let blob = virt().blob();
        let fdt = Fdt::new(&blob).unwrap();
        let mut seen = 0;
        fdt.each(
            |node| node.is_compatible("virtio,mmio"),
            |_| {
                seen += 1;
                false
            },
        )
        .unwrap();
        assert_eq!(seen, 1, "the first answer of `false` must end the walk");
    }

    #[test]
    fn an_address_length_is_inherited_rather_than_assumed() {
        // The same `reg` value read in two widths: one cell means the first
        // word is the whole address, two mean the words are high and then low.
        // Reading a tree in the wrong width is the mistake this test exists to
        // make impossible.
        let narrow = Tree::new()
            .begin("")
            .words("#address-cells", &[1])
            .words("#size-cells", &[0])
            .begin("window@1000")
            .words("reg", &[0x1000])
            .list("compatible", &["test,narrow"])
            .end()
            .end()
            .finish()
            .blob();
        let wide = Tree::new()
            .begin("")
            .words("#address-cells", &[2])
            .words("#size-cells", &[2])
            .begin("window@1000")
            .words("reg", &[0, 0x1000, 0, 0x2000])
            .list("compatible", &["test,wide"])
            .end()
            .end()
            .finish()
            .blob();

        let narrow = Fdt::new(&narrow).unwrap();
        let node = narrow.find_compatible("test,narrow").unwrap().unwrap();
        assert_eq!(node.reg().collect::<Vec<_>>(), vec![(0x1000, 0)]);

        let wide = Fdt::new(&wide).unwrap();
        let node = wide.find_compatible("test,wide").unwrap().unwrap();
        assert_eq!(node.reg().collect::<Vec<_>>(), vec![(0x1000, 0x2000)]);

        // And the counts stay with the node that declared them: the child is
        // read in its parent's widths, not in whatever it declares itself.
        let nested = Tree::new()
            .begin("")
            .words("#address-cells", &[1])
            .words("#size-cells", &[1])
            .begin("parent@1000")
            .words("reg", &[0x1000, 0x10])
            .words("#address-cells", &[2])
            .words("#size-cells", &[2])
            .list("compatible", &["test,parent"])
            .begin("child@0")
            .words("reg", &[0, 0x2000, 0, 0x20])
            .list("compatible", &["test,child"])
            .end()
            .end()
            .end()
            .finish()
            .blob();
        let fdt = Fdt::new(&nested).unwrap();
        let parent = fdt.find_compatible("test,parent").unwrap().unwrap();
        let child = fdt.find_compatible("test,child").unwrap().unwrap();
        assert_eq!(parent.reg().collect::<Vec<_>>(), vec![(0x1000, 0x10)]);
        assert_eq!(child.reg().collect::<Vec<_>>(), vec![(0x2000, 0x20)]);
    }

    #[test]
    fn a_reg_too_short_for_one_window_ends_the_iteration() {
        let blob = Tree::new()
            .begin("")
            .words("#address-cells", &[2])
            .words("#size-cells", &[2])
            .begin("short@1000")
            .words("reg", &[0, 0x1000, 0])
            .list("compatible", &["test,short"])
            .end()
            .end()
            .finish()
            .blob();
        let fdt = Fdt::new(&blob).unwrap();
        let node = fdt.find_compatible("test,short").unwrap().unwrap();
        assert_eq!(node.cells("reg").remaining(), 3);
        assert_eq!(node.reg().collect::<Vec<_>>(), vec![]);
        // Nothing was consumed on the way: the cells are all still there.
        assert_eq!(node.cells("reg").collect::<Vec<_>>(), vec![0, 0x1000, 0]);
    }

    #[test]
    fn a_blob_that_is_not_a_tree_is_refused_by_name() {
        let blob = virt().blob();

        let mut wrong_magic = blob.clone();
        wrong_magic[0] = 0;
        assert!(matches!(Fdt::new(&wrong_magic), Err(Error::Magic(_))));

        assert!(matches!(Fdt::new(&[]), Err(Error::Truncated("header"))));
        assert!(matches!(
            Fdt::new(&blob[..HEADER_SIZE - 1]),
            Err(Error::Truncated("header"))
        ));

        let mut old = blob.clone();
        old[20..24].copy_from_slice(&15u32.to_be_bytes());
        assert_eq!(Fdt::new(&old).unwrap_err(), Error::Version(15));

        // An offset past the end is refused rather than followed.
        let mut past = blob.clone();
        past[8..12].copy_from_slice(&0xffff_0000u32.to_be_bytes());
        assert!(matches!(Fdt::new(&past), Err(Error::Truncated(_))));

        // A block length that runs past the end likewise.
        let mut long = blob.clone();
        long[36..40].copy_from_slice(&0xffff_0000u32.to_be_bytes());
        assert!(matches!(Fdt::new(&long), Err(Error::Truncated(_))));

        assert_eq!(
            Fdt::new(&vec![0u8; MAX_BYTES + 1]).unwrap_err(),
            Error::TooLarge(MAX_BYTES + 1)
        );
    }

    #[test]
    fn an_unclosed_node_is_refused() {
        // A root that opens a child, closes the child, and ends the block with
        // its own node still open. The walk has to *reach* the end to notice:
        // `find` answers as soon as it has its node and never looks at the
        // imbalance behind it, which is why the check in `arch` cross-checks
        // what it read against the machine's own addresses rather than trusting
        // that a tree it could walk was a tree the firmware meant.
        let blob = Tree::new()
            .begin("")
            .begin("child@0")
            .list("compatible", &["test,child"])
            .end()
            .finish()
            .blob();
        let fdt = Fdt::new(&blob).unwrap();
        assert!(fdt.find_compatible("test,child").unwrap().is_some());
        assert_eq!(fdt.each(|_| true, |_| true).unwrap_err(), Error::Unbalanced);
    }

    #[test]
    fn a_property_name_outside_the_strings_block_is_refused() {
        // The root's first property, whose name offset is the third word of its
        // record: `FDT_BEGIN_NODE` and the padded empty name first.
        let mut tree = virt();
        let at = 8 + 8;
        tree.structure[at..at + 4].copy_from_slice(&0xffffu32.to_be_bytes());
        let blob = tree.blob();
        let fdt = Fdt::new(&blob).unwrap();
        assert_eq!(
            fdt.find_compatible("arm,pl011").unwrap_err(),
            Error::NameOffset(0xffff)
        );
    }

    #[test]
    fn a_tree_deeper_than_the_reader_bounds_is_refused() {
        let mut tree = Tree::new();
        for _ in 0..MAX_DEPTH + 1 {
            tree = tree.begin("deep");
        }
        let blob = tree.finish().blob();
        let fdt = Fdt::new(&blob).unwrap();
        assert_eq!(fdt.each(|_| true, |_| true).unwrap_err(), Error::TooDeep);
    }
}
