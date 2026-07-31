#!/usr/bin/env python3
"""Build the ring-3 ELF programs that rust-os loads from disk.

There is no cross-assembler in the toolchain, so this emits x86-64 machine
code directly through a tiny assembler with label fixups, and wraps it in a
static ELF64 executable. Imported by tools/mkfatimg.py; run directly to dump a
summary.

Programs link into the kernel's per-process region (see USER_P4_INDEX in
src/process.rs), so addresses do not fit in 32 bits: data is reached with
rip-relative addressing, which is also why the code is position independent.
"""

from __future__ import annotations

import struct

# Must match src/process.rs.
USER_REGION_START = 64 << 39  # 0x2000_0000_0000
CODE_VADDR = USER_REGION_START + 0x1000
DATA_VADDR = USER_REGION_START + 0x2000
CODE_OFFSET = 0x1000  # file offset of the code segment
DATA_OFFSET = 0x2000

# Syscall numbers (src/usermode.rs).
SYS_EXIT = 0
SYS_WRITE = 1
SYS_UPTIME_MS = 2
SYS_GETPID = 3

# ModRM reg/rm encodings.
RAX, RCX, RDX, RBX, RSP, RBP, RSI, RDI = range(8)


class Asm:
    """Emits machine code, resolving rip-relative label references."""

    def __init__(self, base: int):
        self.base = base
        self.code = bytearray()
        self.labels: dict[str, int] = {}
        self.fixups: list[tuple[int, str, int]] = []

    # -- plumbing --------------------------------------------------------
    def label(self, name: str) -> None:
        self.labels[name] = self.base + len(self.code)

    def emit(self, *values: int) -> None:
        self.code += bytes(values)

    def _rip_fixup(self, label: str, trailing: int = 0) -> None:
        """Reserve a 32-bit rip-relative displacement to `label`.

        `trailing` counts instruction bytes that follow the displacement (an
        immediate, typically). rip is the address of the *next* instruction,
        so those bytes are part of the distance -- forgetting them is an
        off-by-`trailing` bug that silently targets the wrong address.
        """
        self.fixups.append((len(self.code), label, trailing))
        self.code += b"\0\0\0\0"

    def data(self, raw: bytes) -> None:
        self.code += raw

    def finish(self) -> bytes:
        for offset, label, trailing in self.fixups:
            target = self.labels[label]
            # rip points at the *next* instruction: past the disp32 and past
            # any immediate that follows it.
            next_insn = self.base + offset + 4 + trailing
            struct.pack_into("<i", self.code, offset, target - next_insn)
        return bytes(self.code)

    # -- instructions ----------------------------------------------------
    def mov_reg_imm32(self, reg: int, value: int) -> None:
        """mov r64, imm32 (sign-extended)."""
        self.emit(0x48, 0xC7, 0xC0 | reg)
        self.code += struct.pack("<i", value)

    def lea_rip(self, reg: int, label: str) -> None:
        """lea r64, [rip + label]."""
        self.emit(0x48, 0x8D, 0x05 | (reg << 3))
        self._rip_fixup(label)

    def movzx_reg_byte_rip(self, reg: int, label: str) -> None:
        """movzx r32, byte [rip + label]."""
        self.emit(0x0F, 0xB6, 0x05 | (reg << 3))
        self._rip_fixup(label)

    def mov_byte_rip_imm(self, label: str, value: int) -> None:
        """mov byte [rip + label], imm8."""
        self.emit(0xC6, 0x05)
        self._rip_fixup(label, trailing=1)  # the imm8 below
        self.emit(value)

    def mov_reg_from_cs(self, reg: int) -> None:
        """mov r32, cs -- the low two bits are the current privilege level."""
        self.emit(0x8C, 0xC8 | reg)

    def and_reg_imm8(self, reg: int, value: int) -> None:
        self.emit(0x83, 0xE0 | reg, value)

    def mov_reg_reg(self, dst: int, src: int) -> None:
        """mov r32, r32."""
        self.emit(0x89, 0xC0 | (src << 3) | dst)

    def add_reg_reg(self, dst: int, src: int) -> None:
        """add r32, r32."""
        self.emit(0x01, 0xC0 | (src << 3) | dst)

    def xor_reg_self(self, reg: int) -> None:
        self.emit(0x31, 0xC0 | (reg << 3) | reg)

    def load_via_rax(self) -> None:
        """mov rax, [rax] -- faults when rax is unmapped."""
        self.emit(0x48, 0x8B, 0x00)

    def syscall(self) -> None:
        self.emit(0xCD, 0x80)  # int 0x80

    def hang(self) -> None:
        self.emit(0xEB, 0xFE)  # jmp $


def build_elf(code: bytes, data: bytes = b"", bss_size: int = 0,
              code_vaddr: int = CODE_VADDR) -> bytes:
    """Wrap machine code (and optional writable data) in a static ELF64."""
    segments = [
        # (offset, vaddr, filesz, memsz, flags: 4=R 2=W 1=X)
        (CODE_OFFSET, code_vaddr, len(code), len(code), 4 | 1),
    ]
    if data or bss_size:
        segments.append(
            (DATA_OFFSET, DATA_VADDR, len(data), len(data) + bss_size, 4 | 2)
        )

    ehdr = bytearray(64)
    ehdr[0:4] = b"\x7fELF"
    ehdr[4] = 2  # ELFCLASS64
    ehdr[5] = 1  # ELFDATA2LSB
    ehdr[6] = 1  # EV_CURRENT
    struct.pack_into("<H", ehdr, 16, 2)  # e_type = ET_EXEC
    struct.pack_into("<H", ehdr, 18, 0x3E)  # e_machine = x86-64
    struct.pack_into("<I", ehdr, 20, 1)  # e_version
    struct.pack_into("<Q", ehdr, 24, code_vaddr)  # e_entry
    struct.pack_into("<Q", ehdr, 32, 64)  # e_phoff
    struct.pack_into("<H", ehdr, 52, 64)  # e_ehsize
    struct.pack_into("<H", ehdr, 54, 56)  # e_phentsize
    struct.pack_into("<H", ehdr, 56, len(segments))  # e_phnum

    phdrs = bytearray()
    for offset, vaddr, filesz, memsz, flags in segments:
        phdr = bytearray(56)
        struct.pack_into("<I", phdr, 0, 1)  # PT_LOAD
        struct.pack_into("<I", phdr, 4, flags)
        struct.pack_into("<Q", phdr, 8, offset)
        struct.pack_into("<Q", phdr, 16, vaddr)
        struct.pack_into("<Q", phdr, 24, vaddr)  # p_paddr (unused)
        struct.pack_into("<Q", phdr, 32, filesz)
        struct.pack_into("<Q", phdr, 40, memsz)
        struct.pack_into("<Q", phdr, 48, 0x1000)  # p_align
        phdrs += phdr

    size = DATA_OFFSET + len(data) if len(segments) > 1 else CODE_OFFSET + len(code)
    image = bytearray(size)
    image[0:64] = ehdr
    image[64 : 64 + len(phdrs)] = phdrs
    image[CODE_OFFSET : CODE_OFFSET + len(code)] = code
    if len(segments) > 1:
        image[DATA_OFFSET : DATA_OFFSET + len(data)] = data
    return bytes(image)


# ------------------------------------------------------------- programs --


def prog_hello() -> bytes:
    """write(msg); exit(0)"""
    a = Asm(CODE_VADDR)
    message = b"Hello from a real ELF process!\n"
    a.mov_reg_imm32(RAX, SYS_WRITE)
    a.lea_rip(RDI, "msg")
    a.mov_reg_imm32(RSI, len(message))
    a.syscall()
    a.mov_reg_imm32(RAX, SYS_EXIT)
    a.mov_reg_imm32(RDI, 0)
    a.syscall()
    a.hang()
    a.label("msg")
    a.data(message)
    return build_elf(a.finish())


def prog_ring3() -> bytes:
    """exit(cs & 3) -- exit code 3 is proof the program ran in ring 3."""
    a = Asm(CODE_VADDR)
    a.mov_reg_from_cs(RAX)
    a.and_reg_imm8(RAX, 3)
    a.mov_reg_reg(RDI, RAX)
    a.xor_reg_self(RAX)  # SYS_EXIT
    a.syscall()
    a.hang()
    return build_elf(a.finish())


def prog_bss() -> bytes:
    """Prove .bss is zero-filled and writable: exit(0 + 5) == 5."""
    a = Asm(CODE_VADDR)
    a.movzx_reg_byte_rip(RDI, "flag")  # expected 0 (zero-filled .bss)
    a.mov_byte_rip_imm("flag", 5)  # writable data segment
    a.movzx_reg_byte_rip(RAX, "flag")  # reads back 5
    a.add_reg_reg(RDI, RAX)  # 0 + 5
    a.xor_reg_self(RAX)  # SYS_EXIT
    a.syscall()
    a.hang()
    # `flag` lives in .bss: no file bytes, one byte of memory.
    a.labels["flag"] = DATA_VADDR
    return build_elf(a.finish(), data=b"", bss_size=16)


def prog_syscalls() -> bytes:
    """exit(getpid()) -- proves a non-trivial syscall return value."""
    a = Asm(CODE_VADDR)
    a.mov_reg_imm32(RAX, SYS_GETPID)
    a.syscall()
    a.mov_reg_reg(RDI, RAX)
    a.xor_reg_self(RAX)  # SYS_EXIT
    a.syscall()
    a.hang()
    return build_elf(a.finish())


def prog_crash() -> bytes:
    """Dereference a null pointer, to prove the kernel contains the fault."""
    a = Asm(CODE_VADDR)
    a.xor_reg_self(RAX)
    a.load_via_rax()  # mov rax, [0] -> page fault from ring 3
    a.hang()
    return build_elf(a.finish())


def prog_evil() -> bytes:
    """A well-formed ELF asking to be loaded over kernel memory.

    The loader must reject it before allocating anything.
    """
    a = Asm(0x20_0000)
    a.xor_reg_self(RAX)
    a.syscall()
    a.hang()
    return build_elf(a.finish(), code_vaddr=0x20_0000)


PROGRAMS = {
    "HELLO.ELF": prog_hello,
    "RING3.ELF": prog_ring3,
    "BSS.ELF": prog_bss,
    "SYSCALL.ELF": prog_syscalls,
    "CRASH.ELF": prog_crash,
    "EVIL.ELF": prog_evil,
}


def build_all() -> dict[str, bytes]:
    return {name: build() for name, build in PROGRAMS.items()}


if __name__ == "__main__":
    for name, image in build_all().items():
        print(f"{name:<12} {len(image):>6} bytes")
