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
  about         show kernel info
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
- Commands: `help`, `clear`, `echo`, `date`, `uptime`, `sleep`, `about`

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

### VGA text driver improvements

- `backspace()` and `clear_screen()` for interactive editing
- **Hardware cursor control** — enabling the cursor and moving it to follow typed output, by
  programming the VGA CRT controller's cursor-shape and cursor-position registers

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
├── rtc.rs            # CMOS real-time clock, wall-clock time
└── task/
    ├── mod.rs            # task abstraction
    ├── executor.rs       # waker-based cooperative executor
    ├── simple_executor.rs
    ├── keyboard.rs       # scancode stream
    └── shell.rs          # interactive shell
tests/                # integration tests, each booted in QEMU
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

The custom target (`x86_64-rust-os.json`) disables the red zone, disables SSE/MMX and uses
soft-float — floating-point state can't be assumed safe inside interrupt handlers — and sets
`panic = "abort"`, since unwinding needs runtime support the kernel doesn't have.

---

## Roadmap

Planned work, roughly in order of ambition:

- [ ] Command history and scrollback in the shell
- [ ] Kernel-maintained clock (seed from the RTC once at boot, advance with PIT ticks)
- [ ] **Preemptive multitasking** — kernel threads with separate stacks and timer-driven context
      switching, moving past the current cooperative model
- [ ] **User mode (ring 3) and system calls**
- [ ] **A filesystem** — ATA/virtio block driver plus FAT
- [ ] **ELF loader and real processes**
- [ ] Networking — NIC driver and a minimal TCP/IP stack

---

## Acknowledgements

- **[Philipp Oppermann](https://os.phil-opp.com/)** for *Writing an OS in Rust*, which this kernel
  is built on top of. It is a genuinely outstanding piece of technical writing.
- **[The OSDev Wiki](https://wiki.osdev.org/)** for hardware documentation on the PIT, CMOS RTC,
  and VGA CRT controller.
- The [`x86_64`](https://crates.io/crates/x86_64) crate and the wider Rust embedded/OSDev ecosystem.
