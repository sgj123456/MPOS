/*
 * Position-independent kernel image, as `mpos-boot` requires. There is
 * deliberately no absolute base address: the loader picks where the segments
 * land, maps them at KERNEL_VIRT_BASE, and applies the R_X86_64_RELATIVE
 * relocations before jumping to _start.
 *
 * The image is otherwise an ordinary ELF: the loader reads it with no protocol
 * of ours in it, because there is nothing left to negotiate by the time it is
 * read. What was a set of request structures under Limine is now the handoff
 * record the loader passes in a register.
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
