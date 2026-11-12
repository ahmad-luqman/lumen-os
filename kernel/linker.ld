/* Lumen kernel link layout (see docs/ARCHITECTURE.md, "Memory layout").
 *
 * The kernel is linked at the top 2 GiB of the address space
 * (0xffffffff80000000), which is what the kernel code model requires.
 * Each segment is page-aligned with its own permissions: Limine maps text
 * R-X, rodata R--, data RW- from the PHDRs, and the kernel's own page tables
 * (kernel/src/mm/paging.rs) map [__kernel_start, __rodata_start) R-X,
 * [__rodata_start, __data_start) R-- and [__data_start, __kernel_end) RW-.
 */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

PHDRS
{
    text    PT_LOAD FLAGS((1 << 0) | (1 << 2));   /* R-X */
    rodata  PT_LOAD FLAGS((1 << 2));              /* R-- */
    data    PT_LOAD FLAGS((1 << 1) | (1 << 2));   /* RW- */
}

SECTIONS
{
    . = 0xffffffff80000000;
    __kernel_start = .;

    .text : {
        __text_start = .;
        *(.text .text.*)
        __text_end = .;
    } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __rodata_start = .;

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    /* In-kernel test registry: every `ktest!` places a descriptor here. */
    .ktests : {
        __ktests_start = .;
        KEEP(*(.ktests))
        __ktests_end = .;
    } :rodata

    /* Precompiled compiler_builtins calls `memcpy` and friends through the
     * GOT. With static relocation its entries are link-time constants, so it
     * is read-only data. Without this rule the linker placed it after
     * __kernel_end, outside the image the page tables and the frame
     * allocator's reservation cover (`make audit` checks every allocated
     * section is inside). */
    .got : {
        *(.got .got.*)
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __data_start = .;

    .data : {
        /* Limine scans for requests between these markers. */
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
        *(.data .data.*)
    } :data

    .bss : {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    } :data

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
