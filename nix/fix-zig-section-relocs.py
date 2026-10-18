# Zig 0.16 writes compiler_rt.o with some SECTION symbols whose st_shndx is
# SHN_UNDEF; rust-lld reports a relocation against one as a nameless undefined
# symbol. Section symbols are emitted in section order, so the missing index
# is the one non-relocation section between the two neighbouring section
# symbols. When several fit, the symbol is left alone. Usage:
# fix-zig-section-relocs.py OBJECT
import struct
import sys

SHT_RELA, SHT_REL, STT_SECTION = 4, 9, 3

path = sys.argv[1]
data = bytearray(open(path, "rb").read())
assert data[:4] == b"\x7fELF" and data[4] == 2 and data[5] == 1, "ELF64 LE only"

e_shoff, = struct.unpack_from("<Q", data, 0x28)
e_shentsize, e_shnum, _ = struct.unpack_from("<HHH", data, 0x3A)
types, symtab = [], None
for i in range(e_shnum):
    _, typ, _, _, off, size, _, _, _, _ = struct.unpack_from(
        "<IIQQQQIIQQ", data, e_shoff + i * e_shentsize
    )
    types.append(typ)
    if typ == 2:
        symtab = (off, size)

off, size = symtab
entries = [off + i * 24 for i in range(size // 24)]


def sym(e):
    _, info, _, shndx = struct.unpack_from("<IBBH", data, e)
    return info & 0xF, shndx


fixed = 0
for i, e in enumerate(entries):
    typ, shndx = sym(e)
    if typ != STT_SECTION or shndx != 0:
        continue
    prev = next(sym(entries[j])[1] for j in range(i - 1, -1, -1) if sym(entries[j]) [0] == STT_SECTION and sym(entries[j])[1])
    after = next(sym(entries[j])[1] for j in range(i + 1, len(entries)) if sym(entries[j])[0] == STT_SECTION and sym(entries[j])[1])
    gap = [k for k in range(prev + 1, after) if types[k] not in (SHT_RELA, SHT_REL)]
    if len(gap) != 1:
        print(f"{path}: section symbol {i} left alone, sections {gap} all fit")
        continue
    struct.pack_into("<H", data, e + 6, gap[0])
    fixed += 1

open(path, "wb").write(data)
print(f"{path}: restored {fixed} section symbols")
