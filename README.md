# rust-os

A small **x86_64 operating system kernel written in Rust** — no standard library, no host OS
underneath, no runtime. It boots on bare metal (or QEMU), sets up its own memory management and
interrupt handling, and drops you into an interactive shell.

```
rust-os shell -- type `help` for a list of commands.
> help
available commands:
  help          show this message
  clear         clear the screen
  echo <text>   print <text> back
  date          show the current date and time (UTC)
  uptime        show time since boot
  sleep <ms>    pause for <ms> milliseconds
  spawn         start a busy-loop kernel thread
  threads       list kernel threads
  user          run the embedded ring-3 demo program
  ls [path]     list a directory on the disk
  cat <path>    print a file from the disk
  disk          show mounted volume info
  about         show kernel info
keys: up/down browse history, PgUp/PgDn scroll output
> ls
  HELLO.TXT          33
  README.TXT        295
  POEM.TXT         1960
  DOCS            <DIR>
4 entries
> cat hello.txt
Hello from the FAT16 filesystem!
> user
Hello from ring 3! (write syscall via int 0x80)
user program exited with code 3
> spawn
spawned thread 1 (busy loop; watch `threads`)
> threads
   0  running (shell/executor)
   1  ready
work counter: 30761266
> date
2026-07-19 14:33:07 UTC
> uptime
up 42.360 s
> sleep 500
slept 500 ms
> echo hello from ring 0
hello from ring 0
>
```

> **Attribution, up front:** the foundation of this kernel was built by following Philipp
> Oppermann's excellent [*Writing an OS in Rust*](https://os.phil-opp.com/) (`blog_os`) series.
> That series is responsible for everything in the [Foundation](#foundation)
> section below. Everything in [Beyond the tutorial](#beyond-the-tutorial) was
> built after the series ended, without a guide — working from OSDev references and hardware
> datasheets instead.

> **Status:** actively developed. New features are landing regularly and this README is updated as
> they do. See the [Roadmap](#roadmap).

---

## Beyond the tutorial

These are the features built independently once the tutorial ended. Each one meant reading
hardware documentation directly and reasoning about correctness in an environment with no
debugger, no `println` safety net, and no OS to catch mistakes.

### Interactive shell

A real command interpreter running as an async task on the kernel's cooperative executor.

- Line editing with echo and backspace, built on the raw scancode stream
- Tokenised command dispatch with argument parsing
- **Async command execution** — commands can `.await`, so `sleep 2000` suspends the shell task for
  two seconds without blocking the executor or stalling interrupt handling
- **Command history** — up/down arrows recall the last 50 commands, stashing the in-progress line
  so arrowing back down returns to what was being typed
- **Screen scrollback** — PageUp/PageDown page through the last 200 lines of output; any new
  output or keystroke snaps back to the live view
- Commands: `help`, `clear`, `echo`, `date`, `uptime`, `sleep`, `spawn`, `threads`, `user`,
  `ls`, `cat`, `disk`, `about`

### PIT-backed monotonic clock and async `sleep`

The Programmable Interval Timer reprogrammed from its default ~18.2 Hz to a known 100 Hz, driving
a monotonic tick counter and a proper async timer.

- `sleep(ms)` returns a future that registers its waker against a deadline and is woken by the
  timer interrupt — no busy-waiting, no blocking
- `uptime_ms()` / `ticks()` for monotonic time

### Wall-clock time from the CMOS RTC

Real date and time read directly from the real-time clock over the CMOS index/data ports.

Getting this right meant handling three separate hardware quirks that each silently corrupt the
result — see [Engineering notes](#engineering-notes).

### Kernel-maintained clock

How real kernels keep time: the RTC is read **exactly once, at boot**, capturing a wall-clock
reference alongside the PIT tick count. From then on `date` is derived from
`boot time + elapsed ticks` — an atomic load and calendar arithmetic, with no port I/O and no
spinning on RTC update flags.

- Unix-seconds ↔ civil-date conversion via Howard Hinnant's `days_from_civil` /
  `civil_from_days` algorithms, handling leap years correctly across century boundaries
- Calendar math covered by in-kernel unit tests (epoch, leap day, year boundary, known timestamps)

### Preemptive multitasking

Kernel threads with their own stacks, switched by the timer interrupt — the leap from "tasks
politely take turns" to "the kernel decides who runs."

- **Hand-written context switch** in a dedicated assembly file (`threads.s`): callee-saved
  registers pushed, stack pointers swapped, execution resumes mid-function in another thread
- **Round-robin scheduling** on every PIT tick (10 ms quantum); a thread that never yields
  still can't monopolise the CPU
- New threads bootstrapped by hand-crafting their initial stack frame so the first context
  switch "returns" into a trampoline that enables interrupts and calls the entry function
- The boot flow (and the async executor with the shell) runs on as thread 0 — cooperative
  async tasks *within* a thread, preemptive threads around them
- **Proven by an integration test**: a busy-loop thread that never yields, watched by a main
  flow that never yields either — only timer preemption can interleave them (`tests/preemption.rs`)
- Shell: `spawn` starts a busy worker; `threads` lists thread states live

### User mode (ring 3) + system calls

Code running with user privileges, talking to the kernel only through a syscall interface — the
kernel/userspace boundary that everything else in an OS is built around.

- **Ring-3 GDT segments** and a TSS privilege stack (RSP0) so interrupts arriving during user
  code switch to a kernel stack safely
- **`int 0x80` syscall gate** (DPL 3) with a register-based ABI: `rax` = number,
  `rdi`/`rsi`/`rdx` = args — syscalls: `exit`, `write`, `uptime_ms`
- `write` validates that user pointers lie inside the user region before touching them
- User pages mapped `USER_ACCESSIBLE` — including widening the *pre-existing parent* page-table
  entries, the step everyone forgets (see [Engineering notes](#engineering-notes))
- **setjmp/longjmp-style entry**: entering user mode saves the kernel context; the exit syscall
  (or a fault) restores it as if the call had returned, carrying the exit code
- **Fault containment**: a page fault or GPF from ring 3 kills the user program and returns to
  the shell — a crashing user program cannot take the kernel down
- No ELF loader yet, so the demo program is hand-written assembly embedded in the kernel and
  copied into user pages; it exits with its own CPL, and an integration test asserts the exit
  code is 3 — proof it ran in ring 3
- Preemption keeps working while user code runs: kernel threads and ring 3 interleave

### Filesystem: ATA disk driver + FAT16

Real files on a real (virtual) disk, read by the kernel's own driver and filesystem parser — no
firmware calls, no libraries.

- **ATA PIO driver** — task-file registers, LBA28 addressing, IDENTIFY and READ SECTORS,
  polling mode with every hardware wait bounded
- **Read-only FAT16** — BPB parsing and validation, the fixed root-directory region,
  subdirectory cluster chains, 8.3 short names, case-insensitive path resolution
- **Zero allocation in the whole filesystem stack** — the kernel heap is 100 KiB, so `cat` on a
  file of any size streams through a single 512-byte stack buffer and a callback rather than
  buffering the file
- **Untrusted-input discipline** — the BPB is validated before use, cluster numbers are
  range-checked, and every chain walk is bounded so a corrupt or cyclic FAT cannot hang the
  kernel
- Probes both ATA drives and mounts whichever holds a real FAT16 volume, so the boot disk
  (whose sector 0 also ends in `0x55AA`) is correctly rejected
- Shell: `ls [path]`, `cat <path>`, `disk`
- The volume is built by [`tools/mkfatimg.py`](tools/mkfatimg.py) and verified end-to-end by
  `tests/filesystem.rs`, which asserts on exact file contents, sizes, and a multi-cluster chain

### VGA text driver improvements

- `backspace()` and `clear_screen()` for interactive editing
- **Hardware cursor control** — enabling the cursor and moving it to follow typed output, by
  programming the VGA CRT controller's cursor-shape and cursor-position registers
- **Scrollback ring buffer** — rows scrolling off the top are archived into a fixed 200-line ring
  (no heap allocation on the write path), with a paged history view that snapshots and restores
  the live screen

---

## Foundation

Built by following [`blog_os`](https://os.phil-opp.com/). Credit for the design of this layer goes
to that series.

| Area | What it does |
|---|---|
| **Freestanding binary** | `#![no_std]`, custom target spec, no runtime, custom entry point |
| **VGA text output** | Memory-mapped text buffer driver with `print!`/`println!` macros |
| **Serial output** | UART 16550 driver, used to report test results to the host |
| **Testing** | Custom test framework running integration tests inside QEMU |
| **CPU exceptions** | Interrupt Descriptor Table, breakpoint and page-fault handlers |
| **Double faults** | GDT + TSS with an Interrupt Stack Table, so stack overflows fault safely |
| **Hardware interrupts** | 8259 PIC configuration, timer and keyboard IRQs |
| **Paging** | Virtual memory, page table traversal, physical frame allocator from the bootloader memory map |
| **Heap allocation** | Mapped kernel heap backed by a linked-list allocator, enabling `alloc` |
| **Async/await** | Cooperative task executor with proper `Waker` support |

---

## Engineering notes

The parts that were genuinely tricky, and why:

**Interrupt-safe locking.** On a single core, if a task holds a spinlock when an interrupt fires
and the handler tries to take the same lock, the kernel deadlocks permanently. Every lock shared
with an interrupt handler is therefore acquired inside `without_interrupts`, which makes the
critical section atomic with respect to the handler.

**No allocation in interrupt context.** The timer handler runs on every tick and must never
allocate or block. The sleeper registry is structured so that waking a task only pushes an ID onto
a pre-allocated queue.

**Closing a lost-wakeup race.** A timer tick landing between "check the deadline" and "register the
waker" would strand a sleeping task forever. `Sleep::poll` re-checks the deadline *after*
registering, which closes the window.

**Reading the RTC without getting garbage.** Three independent hazards, each of which produces
plausible-looking but wrong timestamps:
- The chip can be *mid-update* when read, tearing a timestamp across a tick — handled by waiting
  out the update-in-progress flag, then reading until two consecutive reads agree.
- Values are usually **BCD**, not binary, so `0x25` means 25 — a naive read reports hour 37.
- In 12-hour mode the high bit of the hour register is a PM flag, which must be stripped *before*
  BCD conversion or it corrupts the digits.

**Bounded hardware waits.** Every spin loop against hardware has an iteration ceiling, so a
misbehaving or absent device degrades instead of hanging the kernel.

**An interrupt you don't want is still an interrupt you must handle.** The first disk read
double-faulted, apparently deep inside a port write. The cause was neither the port
nor the stack — the ATA controller asserts IRQ 14 on command completion, and an interrupt whose
IDT entry is absent escalates into a double fault. A polling driver still has to account for the
interrupts it is ignoring. Fixed at both ends: the driver sets nIEN so devices stop asserting
the line, and the IRQ is given a handler anyway, because a stray interrupt should be logged and
acknowledged rather than fatal. (Acknowledging matters independently: an un-EOI'd IRQ blocks
every lower-priority one, the timer included.)

**When "always disable interrupts around a shared lock" is the wrong answer.** Every other lock
in this kernel that an interrupt handler touches is taken inside `without_interrupts`. The disk
lock deliberately is not: a sector read can spin for milliseconds, and holding interrupts off
that long would stall the timer, stopping both the clock and preemption. No handler touches ATA,
so a plain spinlock held with interrupts *enabled* is correct — contention resolves through
preemption, exactly like the heap allocator's lock. The rule is "match the lock discipline to
who actually contends," not "disable interrupts everywhere."

**The page-table flag everyone forgets.** Mapping a user page `USER_ACCESSIBLE` isn't enough:
every parent level of the page-table walk (P4 → P3 → P2) must carry the flag too, or the CPU
faults the walk at that level. The mapping API only applies flags to tables it *creates* — the
bootloader's pre-existing entries for the low address space had to be widened by hand. The
resulting debug session (user rip faulting on a read of address `0x30`) also surfaced a classic
Intel-syntax assembler trap: `mov rsi, symbol` assembles as a *load from* that address, not the
constant — it needs `offset`.

**Context switching from inside an interrupt handler.** Three ordering rules make preemption
sound: the PIC gets its end-of-interrupt *before* the switch (the next thread runs with the old
thread's interrupt frame still parked on its stack — an unacknowledged PIC would silently stop
all future preemption); the scheduler lock is released *before* the switch (or the next thread
deadlocks on its first `spawn`); and the scheduler never touches the heap in interrupt context —
its run queue and finished-thread list are fixed-capacity, and dead threads' stacks are freed
later, from thread context, because a thread can't free the stack it's standing on.

---

## Layout

```
src/
├── main.rs           # kernel entry point
├── lib.rs            # kernel library, init sequence, test harness
├── vga_buffer.rs     # VGA text driver, hardware cursor
├── serial.rs         # UART 16550, host-side output
├── gdt.rs            # GDT + TSS, double-fault stack
├── interrupts.rs     # IDT, PIC, exception and IRQ handlers
├── memory.rs         # paging, page table walk, frame allocator
├── allocator.rs      # kernel heap
├── time.rs           # PIT clock, tick counter, async sleep
├── rtc.rs            # CMOS real-time clock driver
├── clock.rs          # kernel wall clock: RTC-seeded, PIT-advanced
├── threads.rs        # preemptive kernel threads: scheduler, spawn/exit/reap
├── threads.s         # context switch + thread entry trampoline (assembly)
├── usermode.rs       # ring 3: user page mapping, syscall dispatch, run/exit
├── usermode.s        # syscall entry stub, iretq entry/exit, demo program
├── ata.rs            # ATA PIO disk driver (polling, LBA28)
└── fat.rs            # read-only FAT16: BPB, directories, cluster chains
└── task/
    ├── mod.rs            # task abstraction
    ├── executor.rs       # waker-based cooperative executor
    ├── simple_executor.rs
    ├── keyboard.rs       # scancode stream
    └── shell.rs          # interactive shell
tests/                # integration tests, each booted in QEMU
tools/mkfatimg.py     # builds disk.img, the FAT16 volume the kernel mounts
x86_64-rust-os.json   # custom bare-metal target specification
```

---

## Building and running

**Prerequisites**

- Rust **nightly** (pinned by `rust-toolchain`) — the kernel relies on unstable features and
  builds `core`/`alloc` from source for a custom target
- [QEMU](https://www.qemu.org/) (`qemu-system-x86_64`) on your `PATH`
- The bootimage tooling:

```sh
rustup component add llvm-tools-preview
cargo install bootimage
```

**Run it**

```sh
cargo run          # build a bootable image and boot it in QEMU
cargo build        # build the kernel only
cargo test         # boot each integration test in QEMU, report via serial
```

QEMU is given a second disk, `disk.img` — the FAT16 volume the kernel mounts. It is committed to
the repository so a fresh clone just works; regenerate it with `python tools/mkfatimg.py` after
changing what it contains.

The custom target (`x86_64-rust-os.json`) disables the red zone, disables SSE/MMX and uses
soft-float — floating-point state can't be assumed safe inside interrupt handlers — and sets
`panic = "abort"`, since unwinding needs runtime support the kernel doesn't have.

---

## Roadmap

Planned work, roughly in order of ambition:

- [x] Command history and scrollback in the shell
- [x] Kernel-maintained clock (seed from the RTC once at boot, advance with PIT ticks)
- [x] **Preemptive multitasking** — kernel threads with separate stacks and timer-driven context
      switching, moving past the current cooperative model
- [x] **User mode (ring 3) and system calls**
- [x] **A filesystem** — ATA block driver plus FAT16
- [ ] **ELF loader and real processes**
- [ ] Networking — NIC driver and a minimal TCP/IP stack

---

## Acknowledgements

- **[Philipp Oppermann](https://os.phil-opp.com/)** for *Writing an OS in Rust*, which this kernel
  is built on top of.
- **[The OSDev Wiki](https://wiki.osdev.org/)** for hardware documentation on the PIT, CMOS RTC,
  and VGA CRT controller.
- The [`x86_64`](https://crates.io/crates/x86_64) crate and the wider Rust embedded/OSDev ecosystem.
