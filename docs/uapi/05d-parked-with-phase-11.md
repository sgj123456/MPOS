# UAPI §5.4

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md).
Section numbers are unchanged from the original `UAPI.md`.

### 5.4 The structural work, parked with Phase 11

These are not UAPI defects; they are the refactors that keep the UAPI surface honest as it
grows, and each is named so it is not rediscovered as a shape.

- **B2 — each device's `ioctl` contract moves down to its driver.** The `syscall/linux/fs.rs`
  switchboard is the V-2 edge the architecture audit flags, and the `'K'`/`'E'` families
  that are refused "until B2" are refused *because* of it (`docs/notes/` §11.2, §12.1 A2/A3).
- **The `net` module's `target_os = "none"` gate** means `AF_PACKET`'s parsers and the
  `sockaddr_ll` decode have no host test and no fuzz target. Closing it moves the
  wire-format halves to an ungated pure module (`docs/notes/` §4.5, `ARCHITECTURE.md` V-9/V-11).
