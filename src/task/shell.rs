use alloc::string::String;
use alloc::vec::Vec;
use core::mem;
use core::sync::atomic::{AtomicU64, Ordering};

use futures_util::stream::StreamExt;
use pc_keyboard::{layouts, DecodedKey, HandleControl, KeyCode, Keyboard, ScancodeSet1};

use crate::task::keyboard::ScancodeStream;
use crate::{clock, print, println, threads, time, vga_buffer};

const PROMPT: &str = "> ";
const HISTORY_CAP: usize = 50;

/// Shared work counter for `spawn`ed demo threads, read by `threads`.
static WORK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Demo workload: a thread that never yields. It only makes progress because
/// the timer preempts whoever else is running — watching this counter climb
/// while the shell stays responsive is the whole point.
fn busy_worker() {
    for _ in 0..500_000_000u64 {
        WORK_COUNTER.fetch_add(1, Ordering::Relaxed);
    }
}

/// Past commands, browsable with the up/down arrow keys.
struct History {
    entries: Vec<String>,
    /// Index into `entries` while browsing; `None` when typing a fresh line.
    browse: Option<usize>,
    /// The unfinished line stashed away when browsing began, restored by
    /// arrowing down past the newest entry.
    stash: String,
}

impl History {
    fn new() -> Self {
        History {
            entries: Vec::new(),
            browse: None,
            stash: String::new(),
        }
    }

    /// Saves an executed command and leaves browse mode. Empty lines and
    /// immediate repeats are not recorded.
    fn record(&mut self, line: &str) {
        self.browse = None;
        if line.is_empty() || self.entries.last().is_some_and(|last| last == line) {
            return;
        }
        if self.entries.len() == HISTORY_CAP {
            self.entries.remove(0);
        }
        self.entries.push(String::from(line));
    }

    /// Steps to an older entry, stashing the in-progress line on first press.
    /// Returns the line the prompt should now show.
    fn older(&mut self, current: &str) -> Option<String> {
        let index = match self.browse {
            None if self.entries.is_empty() => return None,
            None => {
                self.stash = String::from(current);
                self.entries.len() - 1
            }
            Some(0) => return None, // already at the oldest entry
            Some(i) => i - 1,
        };
        self.browse = Some(index);
        Some(self.entries[index].clone())
    }

    /// Steps back toward the present; past the newest entry, restores the
    /// stashed in-progress line.
    fn newer(&mut self) -> Option<String> {
        match self.browse {
            None => None,
            Some(i) if i + 1 < self.entries.len() => {
                self.browse = Some(i + 1);
                Some(self.entries[i + 1].clone())
            }
            Some(_) => {
                self.browse = None;
                Some(mem::take(&mut self.stash))
            }
        }
    }
}

/// Swaps the displayed input line for `new`, erasing the old one on screen.
fn replace_line(line: &mut String, new: String) {
    for _ in 0..line.chars().count() {
        vga_buffer::backspace();
    }
    print!("{}", new);
    *line = new;
}

/// A minimal interactive shell.
///
/// Reads decoded key presses from the keyboard scancode stream, buffers a
/// line (echoing characters and handling backspace), and dispatches the line
/// to a command handler when Enter is pressed.
///
/// This task takes ownership of the `ScancodeStream`, so it replaces
/// `keyboard::print_keypresses` rather than running alongside it — only one
/// consumer of the scancode queue can exist.
pub async fn run_shell() {
    let mut scancodes = ScancodeStream::new();
    let mut keyboard = Keyboard::new(
        ScancodeSet1::new(),
        layouts::Us104Key,
        HandleControl::Ignore,
    );
    let mut line = String::new();
    let mut history = History::new();

    println!();
    println!("rust-os shell -- type `help` for a list of commands.");
    print!("{}", PROMPT);

    while let Some(scancode) = scancodes.next().await {
        if let Ok(Some(key_event)) = keyboard.add_byte(scancode) {
            if let Some(key) = keyboard.process_keyevent(key_event) {
                match key {
                    DecodedKey::Unicode('\n') => {
                        println!();
                        history.record(line.trim());
                        execute(line.trim()).await;
                        line.clear();
                        print!("{}", PROMPT);
                    }
                    // Backspace (0x08) or Delete (0x7f): remove the last char.
                    DecodedKey::Unicode('\u{8}' | '\u{7f}') => {
                        if line.pop().is_some() {
                            vga_buffer::backspace();
                        }
                    }
                    // Ignore other control chars; echo everything printable.
                    DecodedKey::Unicode(c) if !c.is_control() => {
                        line.push(c);
                        print!("{}", c);
                    }
                    DecodedKey::RawKey(KeyCode::ArrowUp) => {
                        if let Some(older) = history.older(&line) {
                            replace_line(&mut line, older);
                        }
                    }
                    DecodedKey::RawKey(KeyCode::ArrowDown) => {
                        if let Some(newer) = history.newer() {
                            replace_line(&mut line, newer);
                        }
                    }
                    DecodedKey::RawKey(KeyCode::PageUp) => vga_buffer::scroll_page_up(),
                    DecodedKey::RawKey(KeyCode::PageDown) => vga_buffer::scroll_page_down(),
                    _ => {}
                }
            }
        }
    }
}

/// Parses and executes a completed command line.
async fn execute(line: &str) {
    let command = match line.split_whitespace().next() {
        Some(cmd) => cmd,
        None => return, // empty line
    };

    match command {
        "help" => {
            println!("available commands:");
            println!("  help          show this message");
            println!("  clear         clear the screen");
            println!("  echo <text>   print <text> back");
            println!("  date          show the current date and time (UTC)");
            println!("  uptime        show time since boot");
            println!("  sleep <ms>    pause for <ms> milliseconds");
            println!("  spawn         start a busy-loop kernel thread");
            println!("  threads       list kernel threads");
            println!("  about         show kernel info");
            println!("keys: up/down browse history, PgUp/PgDn scroll output");
        }
        "clear" => vga_buffer::clear_screen(),
        "echo" => {
            // Everything after the first whitespace-delimited token.
            let arg = line.splitn(2, char::is_whitespace).nth(1).unwrap_or("");
            println!("{}", arg.trim_start());
        }
        "date" => println!("{} UTC", clock::now()),
        "uptime" => {
            let ms = time::uptime_ms();
            println!("up {}.{:03} s", ms / 1000, ms % 1000);
        }
        "sleep" => {
            let arg = line
                .splitn(2, char::is_whitespace)
                .nth(1)
                .unwrap_or("")
                .trim();
            match arg.parse::<u64>() {
                Ok(ms) => {
                    time::sleep(ms).await;
                    println!("slept {} ms", ms);
                }
                Err(_) => println!("usage: sleep <milliseconds>"),
            }
        }
        "spawn" => match threads::spawn(busy_worker) {
            Ok(id) => println!("spawned thread {} (busy loop; watch `threads`)", id),
            Err(err) => println!("spawn failed: {:?}", err),
        },
        "threads" => {
            let mut buf = [None; threads::MAX_THREADS];
            let count = threads::snapshot(&mut buf);
            for entry in buf.iter().take(count) {
                if let Some((id, state)) = entry {
                    let label = match state {
                        threads::State::Running => "running (shell/executor)",
                        threads::State::Ready => "ready",
                        threads::State::Finished => "finished (awaiting reap)",
                    };
                    println!("  {:>2}  {}", id, label);
                }
            }
            println!("work counter: {}", WORK_COUNTER.load(Ordering::Relaxed));
        }
        "about" => {
            println!(
                "rust-os v{} -- a hobby kernel built on blog_os, then extended.",
                env!("CARGO_PKG_VERSION")
            );
        }
        other => println!("unknown command: `{}` (try `help`)", other),
    }
}
