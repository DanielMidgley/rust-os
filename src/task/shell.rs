use crate::task::keyboard::ScancodeStream;
use crate::{print, println, vga_buffer};
use alloc::string::String;
use futures_util::stream::StreamExt;
use pc_keyboard::{layouts, DecodedKey, HandleControl, Keyboard, ScancodeSet1};

const PROMPT: &str = "> ";

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

    println!();
    println!("rust-os shell -- type `help` for a list of commands.");
    print!("{}", PROMPT);

    while let Some(scancode) = scancodes.next().await {
        if let Ok(Some(key_event)) = keyboard.add_byte(scancode) {
            if let Some(key) = keyboard.process_keyevent(key_event) {
                match key {
                    DecodedKey::Unicode(character) => handle_char(&mut line, character),
                    DecodedKey::RawKey(_) => {} // arrows etc. ignored for now
                }
            }
        }
    }
}

/// Feeds a single decoded character into the line buffer, echoing to screen.
fn handle_char(line: &mut String, c: char) {
    match c {
        '\n' => {
            println!();
            run_command(line.trim());
            line.clear();
            print!("{}", PROMPT);
        }
        // Backspace (0x08) or Delete (0x7f): remove the last char if any.
        '\u{8}' | '\u{7f}' => {
            if line.pop().is_some() {
                vga_buffer::backspace();
            }
        }
        // Ignore other control characters; echo everything printable.
        c if !c.is_control() => {
            line.push(c);
            print!("{}", c);
        }
        _ => {}
    }
}

/// Parses and executes a completed command line.
fn run_command(line: &str) {
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
            println!("  about         show kernel info");
        }
        "clear" => vga_buffer::clear_screen(),
        "echo" => {
            // Everything after the first whitespace-delimited token.
            let arg = line.splitn(2, char::is_whitespace).nth(1).unwrap_or("");
            println!("{}", arg.trim_start());
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
