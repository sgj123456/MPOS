/*
 * The aarch64 kernel image.
 *
 * Unlike the x86_64 script this one names an address. The bring-up path (stage
 * 1b) enters the image with the MMU off, executing at whatever address the
 * firmware or `-kernel` placed it at, so the link address has to be the load
 * address: QEMU virt's RAM starts at 0x4000_0000 and the conventional kernel
 * offset leaves the first half-megabyte to firmware. Stage 1d replaces the
 * fixed address with the higher-half layout once the kernel installs its own
 * tables (TTBR1), and stage 1e's loader places the segments itself, exactly as
 * the x86_64 loader does - at which point this base moves above the direct map.
 *
 * The section symbols are the x86_64 script's, name for name. `kernel_sections`
 * in `src/arch/aarch64/mod.rs` reads them the way its x86_64 twin reads its
 * own, so the mapping *plan* in `mm::vm` stays shared and only the page-table
 * format is the machine's.
 */

ENTRY(_start)

SECTIONS
{
    /* Room for the ELF headers before the first section, so `__text_start` is
       really the first byte of code and not the program header table. */
    . = 0x40080000 + SIZEOF_HEADERS;
    executable_start = . - SIZEOF_HEADERS;

    __text_start = .;
    .text : {
        *(.text .text.*)
    }
    . = ALIGN(4096);
    __text_end = .;

    __rodata_start = .;
    .rodata : {
        *(.rodata .rodata.*)
    }
    . = ALIGN(4096);
    __rodata_end = .;

    __data_start = .;
    .data : {
        *(.data .data.*)
    }
    . = ALIGN(4096);
    __bss_start = .;
    .bss (NOLOAD) : {
        *(.bss .bss.*)
        *(COMMON)
    }
    . = ALIGN(4096);
    __bss_end = .;

    /* The boot stack, in a section of its own rather than in `.bss`: the entry
       zeroes `.bss` and is already running on this stack when it does, so the
       two ranges must not overlap. It is `.bss`-like (NOLOAD) and part of the
       writable image the kernel maps. */
    .stack (NOLOAD) : ALIGN(16) {
        __stack_bottom = .;
        . += 64 * 1024;
        __stack_top = .;
    }

    kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.comment)
        *(.interp)
        *(.dynamic)
    }
}
