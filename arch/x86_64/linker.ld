/*
 * Position-independent kernel image, as required by Limine base revision 3.
 * There is deliberately no absolute base address: Limine picks the load base
 * and applies the R_X86_64_RELATIVE relocations before jumping to _start.
 *
 * The .limine_requests* sections must appear in this exact linear order,
 * because Limine discovers requests by scanning the loaded image for the
 * start marker, then the base revision tag, then COMMON_MAGIC-prefixed
 * requests, stopping at the end marker.
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
        KEEP(*(.limine_requests_start_marker))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end_marker))
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
           page the mapping would reach past the memory Limine reserved for us. */
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
