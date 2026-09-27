pub(crate) fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().expect("in-bounds u16"))
}

pub(crate) fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("in-bounds u32"))
}

pub(crate) fn set16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn set32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    /// The four helpers are one idea in four spellings: an on-disk word is
    /// little-endian, reads and writes agree, and a word at an offset is the
    /// bytes there and nothing else.
    #[test]
    fn a_word_reads_and_writes_back_at_its_own_offset() {
        let mut buf = [0u8; 8];
        set16(&mut buf, 0, 0x1234);
        set32(&mut buf, 4, 0xdead_beef);
        assert_eq!(&buf, &[0x34, 0x12, 0, 0, 0xef, 0xbe, 0xad, 0xde]);
        assert_eq!(le16(&buf, 0), 0x1234);
        assert_eq!(le32(&buf, 4), 0xdead_beef);
    }

    #[test]
    fn an_offset_word_is_the_bytes_there_and_nothing_else() {
        let buf = [0xff, 0x34, 0x12, 0xff, 0xff, 0x78, 0x56, 0x34, 0x12];
        assert_eq!(le16(&buf, 1), 0x1234);
        assert_eq!(le32(&buf, 5), 0x1234_5678);
    }

    /// A write touches exactly the bytes its word occupies - the neighbours
    /// are how a node image's other fields survive an in-place edit.
    #[test]
    fn a_write_leaves_its_neighbours_alone() {
        let mut buf = [0xaa; 8];
        set16(&mut buf, 2, 0x0102);
        assert_eq!(&buf, &[0xaa, 0xaa, 0x02, 0x01, 0xaa, 0xaa, 0xaa, 0xaa]);
    }
}
