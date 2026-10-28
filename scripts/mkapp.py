#!/usr/bin/env python3
"""Pack a p4os user app (static PIE ELF linked with userapps/p4os-user/link.x)
into a flat app image for the kernel loader.

Usage: mkapp.py ELF OUT [--name NAME]

Image format (little-endian), see abi/src/lib.rs and the design note in
plans/261028-1217-user-mode-apps/design.md:
  64-byte header | load image (file_size bytes) | reloc_count x u32 offsets
At every relocation offset the loader adds the load base to the stored word.
"""
import argparse
import struct
import sys
import zlib

MAGIC = b"P4AP"
VERSION = 1
HEADER_SIZE = 64
REGION_ALIGN = 128
SLOT_SIZE = 64 * 1024
MAX_APP_MEM = 256 * 1024
NAME_LEN = 16

ET_DYN = 3
EM_RISCV = 0xF3
SHF_WRITE, SHF_ALLOC, SHF_EXECINSTR = 1, 2, 4
SHT_SYMTAB, SHT_RELA, SHT_NOBITS, SHT_REL = 2, 4, 8, 9
R_RISCV_RELATIVE = 3


def fail(msg):
    sys.exit(f"mkapp: {msg}")


def parse_elf(data):
    if data[:4] != b"\x7fELF" or data[4] != 1 or data[5] != 1:
        fail("not a 32-bit little-endian ELF")
    (e_type, e_machine, _, e_entry, _, e_shoff, _, _, _, _, e_shentsize, e_shnum,
     _) = struct.unpack_from("<HHIIIIIHHHHHH", data, 16)
    if e_machine != EM_RISCV:
        fail("not a RISC-V ELF")
    if e_type != ET_DYN:
        fail("not position-independent (link with -pie)")
    shdrs = [struct.unpack_from("<10I", data, e_shoff + i * e_shentsize) for i in range(e_shnum)]
    return e_entry, shdrs


def symbols(data, shdrs):
    out = {}
    for sh in shdrs:
        if sh[1] != SHT_SYMTAB:
            continue
        strtab = shdrs[sh[6]]
        for off in range(sh[4], sh[4] + sh[5], 16):
            st_name, st_value = struct.unpack_from("<II", data, off)
            end = data.index(b"\0", strtab[4] + st_name)
            out[data[strtab[4] + st_name:end].decode()] = st_value
    return out


def pack(data, name):
    entry, shdrs = parse_elf(data)
    syms = symbols(data, shdrs)
    try:
        rx_size, mem_size, stack_size = syms["__rx_end"], syms["__mem_end"], syms["__stack_size"]
    except KeyError as e:
        fail(f"missing linker symbol {e} (not linked with the p4os-user script?)")

    image = bytearray()
    for (_, sh_type, sh_flags, sh_addr, sh_offset, sh_size, *_rest) in shdrs:
        if not sh_flags & SHF_ALLOC or sh_size == 0 or sh_addr >= mem_size:
            continue  # not loaded, or dynamic-linking metadata after the image
        if sh_addr + sh_size > mem_size:
            fail(f"section at {sh_addr:#x} crosses the end of memory {mem_size:#x}")
        if sh_flags & SHF_WRITE and sh_addr < rx_size:
            fail(f"writable section at {sh_addr:#x} inside the executable region")
        if sh_flags & SHF_EXECINSTR and sh_addr + sh_size > rx_size:
            fail(f"executable section at {sh_addr:#x} beyond the executable region")
        if sh_type == SHT_NOBITS:
            continue  # bss: the loader hands out zeroed memory
        end = sh_addr + sh_size
        if len(image) < end:
            image.extend(bytes(end - len(image)))
        image[sh_addr:end] = data[sh_offset:sh_offset + sh_size]

    relocs = []
    for sh in shdrs:
        if sh[1] == SHT_REL:
            fail("REL relocations are not supported")
        if sh[1] != SHT_RELA:
            continue
        for off in range(sh[4], sh[4] + sh[5], 12):
            r_offset, r_info, r_addend = struct.unpack_from("<IIi", data, off)
            if r_info & 0xFF != R_RISCV_RELATIVE:
                fail(f"unsupported relocation type {r_info & 0xFF} at {r_offset:#x}")
            if r_offset % 4 or r_offset + 4 > len(image) or r_offset < rx_size:
                fail(f"relocation at {r_offset:#x} outside the writable image")
            struct.pack_into("<I", image, r_offset, r_addend & 0xFFFFFFFF)
            relocs.append(r_offset)

    for what, v in (("__rx_end", rx_size), ("__mem_end", mem_size), ("__stack_size", stack_size)):
        if v % REGION_ALIGN:
            fail(f"{what} = {v:#x} is not {REGION_ALIGN}-byte aligned")
    if not 0 <= entry < rx_size:
        fail(f"entry {entry:#x} outside the executable region")
    if mem_size + stack_size > MAX_APP_MEM:
        fail(f"app needs {mem_size + stack_size} bytes of RAM, limit is {MAX_APP_MEM}")

    body = bytes(image) + b"".join(struct.pack("<I", r) for r in relocs)
    raw_name = name.encode()
    if len(raw_name) > NAME_LEN:
        fail(f"name longer than {NAME_LEN} bytes")
    header = struct.pack("<4sHH16s7I", MAGIC, VERSION, HEADER_SIZE, raw_name, len(image), rx_size,
                         mem_size, stack_size, entry, len(relocs), zlib.crc32(body))
    out = header.ljust(HEADER_SIZE, b"\0") + body
    if len(out) > SLOT_SIZE:
        fail(f"image is {len(out)} bytes, a slot holds {SLOT_SIZE}")
    return out, len(image), len(relocs)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("elf")
    ap.add_argument("out")
    ap.add_argument("--name", help="app name (default: ELF file name)")
    args = ap.parse_args()
    with open(args.elf, "rb") as f:
        data = f.read()
    name = args.name or args.elf.rsplit("/", 1)[-1]
    out, size, nrel = pack(data, name)
    with open(args.out, "wb") as f:
        f.write(out)
    print(f"{args.out}: '{name}' {len(out)} bytes ({size} image, {nrel} relocations)")


if __name__ == "__main__":
    main()
