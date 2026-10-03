/*
 * Position-independent kernel image on aarch64, as `mpos-boot` requires.
 *
 * The same shape as the x86_64 script, and deliberately so: there is no
 * absolute base address, the loader picks where the segments land, maps them at
 * KERNEL_VIRT_BASE and applies the R_AARCH64_RELATIVE relocations before
 * branching to `_start`. That is what makes the image's own absolute addresses
 * - the ones a `static`'s address becomes - point into the higher half, which is
 * the whole reason the loader can drop the low identity map afterwards.
 *
 * There used to be an address here (`0x4008_0000`, QEMU virt's conventional
 * kernel offset) and a `.stack` section beside it. Both belonged to the
 * `-kernel` bring-up of stages 1b-1d, which entered the image with the MMU off
 * and ran on a stack the script cut out of the image. Stage 1e deletes that
 * path: the loader builds the page tables, allocates the stack and branches
 * here, so the image is relocatable and the stack is not the image's.
 *
 * The section symbols are the x86_64 script's, name for name. `kernel_sections`
 * in `src/arch/aarch64/mod.rs` reads them the way its x86_64 twin reads its
 * own, so the mapping *plan* in `mm::vm` stays shared and only the page-table
 * format is the machine's.
 */

ENTRY(_start)

PHDRS
{
    headers PT_PHDR PHDRS;
    text    PT_LOAD FILEHDR PHDRS;
    rodata  PT_LOAD;
    data    PT_LOAD;
    dynamic PT_DYNAMIC;
}

SECTIONS
{
    . = SIZEOF_HEADERS;
    executable_start = . - SIZEOF_HEADERS;

    /* Section boundaries, so the kernel can map its own image with W^X once it
       installs its own page tables. End symbols come after the page alignment,
       which makes every range page-granular. */

    .text : {
        __text_start = .;
        *(.text .text.*)
    } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __text_end = .;

    .rodata : {
        __rodata_start = .;
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __rodata_end = .;

    .data : {
        __data_start = .;
        *(.data .data.*)
    } :data

    .dynamic : {
        *(.dynamic)
    } :data :dynamic

    .bss (NOLOAD) : {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
        /* Round the image up to a whole number of pages. The kernel maps
           __data_start..kernel_end as one range, so if kernel_end fell inside a
           page the mapping would reach past the memory the loader reserved. */
        . = ALIGN(CONSTANT(MAXPAGESIZE));
    } :data

    kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.interp)
        *(.comment)
    }
}
