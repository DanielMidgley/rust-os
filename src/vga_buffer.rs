use core::fmt::{self, Write};

use lazy_static::lazy_static;
use spin::Mutex;
use volatile::Volatile;
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Color {
    Black = 0,
    Blue = 1,
    Green = 2,
    Cyan = 3,
    Red = 4,
    Magenta = 5,
    Brown = 6,
    LightGray = 7,
    DarkGray = 8,
    LightBlue = 9,
    LightGreen = 10,
    LightCyan = 11,
    LightRed = 12,
    Pink = 13,
    Yellow = 14,
    White = 15,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
struct ColorCode(u8);

impl ColorCode {
    fn new(foreground: Color, background: Color) -> ColorCode {
        ColorCode((background as u8) << 4 | (foreground as u8))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct ScreenChar {
    ascii_character: u8,
    color_code: ColorCode,
}

const BUFFER_HEIGHT: usize = 25;
const BUFFER_WIDTH: usize = 80;

struct Buffer {
    chars: [[Volatile<ScreenChar>; BUFFER_WIDTH]; BUFFER_HEIGHT],
}

/// How many lines of history the scrollback ring buffer keeps.
const SCROLLBACK_LINES: usize = 200;

/// Lines scrolled per PageUp/PageDown press (one screen minus a context row).
const SCROLL_PAGE: usize = BUFFER_HEIGHT - 1;

/// All-zero cell used only to zero-initialise the scrollback static; never
/// rendered (rows are gated by `stored` and `saved_screen` is overwritten
/// before use), and all-zero data keeps the 36 KiB static in .bss.
const ZERO_CHAR: ScreenChar = ScreenChar {
    ascii_character: 0,
    color_code: ColorCode(0),
};

/// Ring buffer of lines that have scrolled off the top of the screen, plus
/// the state for viewing them.
///
/// Lock ordering: always WRITER before SCROLLBACK, never the reverse. Both
/// are only locked with interrupts disabled.
struct Scrollback {
    lines: [[ScreenChar; BUFFER_WIDTH]; SCROLLBACK_LINES],
    /// Ring index of the next slot to overwrite.
    next: usize,
    /// How many slots hold real history (saturates at `SCROLLBACK_LINES`).
    stored: usize,
    /// How many lines back the user has scrolled; 0 = live view.
    view_offset: usize,
    /// Snapshot of the live screen, taken when scrolling begins, restored
    /// when it ends.
    saved_screen: [[ScreenChar; BUFFER_WIDTH]; BUFFER_HEIGHT],
}

impl Scrollback {
    fn push_line(&mut self, row: [ScreenChar; BUFFER_WIDTH]) {
        self.lines[self.next] = row;
        self.next = (self.next + 1) % SCROLLBACK_LINES;
        if self.stored < SCROLLBACK_LINES {
            self.stored += 1;
        }
    }
}

static SCROLLBACK: Mutex<Scrollback> = Mutex::new(Scrollback {
    lines: [[ZERO_CHAR; BUFFER_WIDTH]; SCROLLBACK_LINES],
    next: 0,
    stored: 0,
    view_offset: 0,
    saved_screen: [[ZERO_CHAR; BUFFER_WIDTH]; BUFFER_HEIGHT],
});

pub struct Writer {
    column_position: usize,
    color_code: ColorCode,
    buffer: &'static mut Buffer,
    /// Newlines processed so far, capped at `BUFFER_HEIGHT - 1`. Until real
    /// content has reached the top row, the rows scrolling off are blank and
    /// not worth archiving.
    rows_filled: usize,
}

impl Writer {
    pub fn write_byte(&mut self, byte: u8) {
        self.snap_to_live();
        match byte {
            b'\n' => self.new_line(), // newline byte
            byte => {
                // check if current line is full
                if self.column_position >= BUFFER_WIDTH {
                    self.new_line();
                }

                let row = BUFFER_HEIGHT - 1;
                let col = self.column_position;

                // add new ScreenChar to buffer
                let color_code = self.color_code;
                self.buffer.chars[row][col].write(ScreenChar {
                    ascii_character: byte,
                    color_code,
                });
                self.column_position += 1
            }
        }
    }

    fn new_line(&mut self) {
        // Archive the row about to scroll off, once real content has had time
        // to reach the top of the screen.
        if self.rows_filled >= BUFFER_HEIGHT - 1 {
            let mut row = [ZERO_CHAR; BUFFER_WIDTH];
            for (col, cell) in row.iter_mut().enumerate() {
                *cell = self.buffer.chars[0][col].read();
            }
            SCROLLBACK.lock().push_line(row);
        } else {
            self.rows_filled += 1;
        }

        for row in 1..BUFFER_HEIGHT {
            for col in 0..BUFFER_WIDTH {
                let character = self.buffer.chars[row][col].read();
                self.buffer.chars[row - 1][col].write(character);
            }
        }
        self.clear_row(BUFFER_HEIGHT - 1);
        self.column_position = 0;
    }

    fn clear_row(&mut self, row: usize) {
        let blank = ScreenChar {
            ascii_character: b' ',
            color_code: self.color_code,
        };
        for col in 0..BUFFER_WIDTH {
            self.buffer.chars[row][col].write(blank);
        }
    }

    pub fn write_string(&mut self, s: &str) {
        for byte in s.bytes() {
            match byte {
                // printable ASCII byte or newline
                0x20..=0x7e | b'\n' => self.write_byte(byte),
                // not in the printable ASCII range
                _ => self.write_byte(0xfe),
            }
        }
    }

    /// Erases the character to the left of the cursor on the current line.
    ///
    /// Does nothing at the start of a line (we don't wrap back to the
    /// previous row, which keeps line editing simple and predictable).
    pub fn backspace(&mut self) {
        self.snap_to_live();
        if self.column_position > 0 {
            self.column_position -= 1;
            let row = BUFFER_HEIGHT - 1;
            let col = self.column_position;
            let color_code = self.color_code;
            self.buffer.chars[row][col].write(ScreenChar {
                ascii_character: b' ',
                color_code,
            });
            self.update_cursor();
        }
    }

    /// Clears the whole screen and returns the cursor to the top-left.
    pub fn clear_screen(&mut self) {
        self.snap_to_live();
        for row in 0..BUFFER_HEIGHT {
            self.clear_row(row);
        }
        self.column_position = 0;
        self.rows_filled = 0;
        self.update_cursor();
    }

    /// If the user is scrolled back, returns to the live view before any new
    /// output lands, so writes never touch the history view.
    fn snap_to_live(&mut self) {
        let mut scrollback = SCROLLBACK.lock();
        if scrollback.view_offset > 0 {
            restore_live(self, &mut scrollback);
        }
    }

    /// Moves the VGA hardware cursor to the current write position.
    ///
    /// Output always lands on the bottom row, so the linear cursor position
    /// is `(BUFFER_HEIGHT - 1) * BUFFER_WIDTH + column_position`.
    fn update_cursor(&self) {
        let pos = ((BUFFER_HEIGHT - 1) * BUFFER_WIDTH + self.column_position) as u16;
        write_cursor_pos(pos);
    }
}

/// Writes a linear cursor position to CRTC registers 0x0E (high) and 0x0F
/// (low) via the index/data ports (0x3D4/0x3D5). Positions past the end of
/// the screen hide the cursor.
fn write_cursor_pos(pos: u16) {
    unsafe {
        let mut index: Port<u8> = Port::new(0x3D4);
        let mut data: Port<u8> = Port::new(0x3D5);
        index.write(0x0F);
        data.write((pos & 0xFF) as u8);
        index.write(0x0E);
        data.write((pos >> 8) as u8);
    }
}

/// Repaints the screen with the scrolled-back view.
///
/// The virtual document is `stored` history lines followed by the saved live
/// screen; `view_offset` picks the window. The hardware cursor is parked
/// off-screen while history is showing.
fn render_view(writer: &mut Writer, sb: &Scrollback) {
    let top = sb.stored - sb.view_offset;
    for screen_row in 0..BUFFER_HEIGHT {
        let doc_row = top + screen_row;
        let source = if doc_row < sb.stored {
            let ring = (sb.next + SCROLLBACK_LINES - sb.stored + doc_row) % SCROLLBACK_LINES;
            &sb.lines[ring]
        } else {
            &sb.saved_screen[doc_row - sb.stored]
        };
        for (col, &ch) in source.iter().enumerate() {
            writer.buffer.chars[screen_row][col].write(ch);
        }
    }
    write_cursor_pos((BUFFER_HEIGHT * BUFFER_WIDTH) as u16);
}

/// Restores the saved live screen and leaves scrollback mode.
fn restore_live(writer: &mut Writer, sb: &mut Scrollback) {
    sb.view_offset = 0;
    for row in 0..BUFFER_HEIGHT {
        for col in 0..BUFFER_WIDTH {
            writer.buffer.chars[row][col].write(sb.saved_screen[row][col]);
        }
    }
    writer.update_cursor();
}

/// Scrolls the view one page further into history (PageUp).
pub fn scroll_page_up() {
    interrupts::without_interrupts(|| {
        let mut writer = WRITER.lock();
        let mut sb = SCROLLBACK.lock();
        if sb.stored == 0 {
            return;
        }
        if sb.view_offset == 0 {
            // Entering scrollback: snapshot the live screen first.
            for row in 0..BUFFER_HEIGHT {
                for col in 0..BUFFER_WIDTH {
                    sb.saved_screen[row][col] = writer.buffer.chars[row][col].read();
                }
            }
        }
        sb.view_offset = (sb.view_offset + SCROLL_PAGE).min(sb.stored);
        render_view(&mut writer, &sb);
    });
}

/// Scrolls the view one page back toward the present (PageDown).
pub fn scroll_page_down() {
    interrupts::without_interrupts(|| {
        let mut writer = WRITER.lock();
        let mut sb = SCROLLBACK.lock();
        if sb.view_offset == 0 {
            return;
        }
        sb.view_offset = sb.view_offset.saturating_sub(SCROLL_PAGE);
        if sb.view_offset == 0 {
            restore_live(&mut writer, &mut sb);
        } else {
            render_view(&mut writer, &sb);
        }
    });
}

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_string(s);
        self.update_cursor();
        Ok(())
    }
}

// lazy_static! causes static to initialise at runtime instead of compile time
lazy_static! {
    pub static ref WRITER: Mutex<Writer> = Mutex::new(Writer {
        column_position: 0,
        color_code: ColorCode::new(Color::Yellow, Color::Black),
        buffer: unsafe { &mut *(0xb8000 as *mut Buffer) },
        rows_filled: 0,
    });
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::vga_buffer::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

/// Prints the given formatted string to the VGA text buffer
/// through the global WRITER instance
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    interrupts::without_interrupts(|| {
        WRITER.lock().write_fmt(args).unwrap();
    });
}

/// Enables the blinking VGA hardware cursor as an underline near the bottom
/// of the character cell.
///
/// The "Cursor Start" register (index 0x0A) holds the disable bit (5) and the
/// start scanline; "Cursor End" (index 0x0B) holds the end scanline. We clear
/// the disable bit and set scanlines 14..15 for an underscore-style cursor.
/// Registers are accessed via the CRTC index/data ports (0x3D4/0x3D5).
pub fn enable_cursor() {
    unsafe {
        let mut index: Port<u8> = Port::new(0x3D4);
        let mut data: Port<u8> = Port::new(0x3D5);
        // Cursor start: preserve top 2 bits, clear disable bit, start = 14.
        index.write(0x0A);
        let start = data.read() & 0xC0;
        data.write(start | 14);
        // Cursor end: preserve top 3 bits, end = 15.
        index.write(0x0B);
        let end = data.read() & 0xE0;
        data.write(end | 15);
    }
}

/// Erases the character to the left of the cursor via the global WRITER.
pub fn backspace() {
    interrupts::without_interrupts(|| {
        WRITER.lock().backspace();
    });
}

/// Clears the screen via the global WRITER.
pub fn clear_screen() {
    interrupts::without_interrupts(|| {
        WRITER.lock().clear_screen();
    });
}






// Tests

#[test_case]
fn test_println_simple() {
    println!("test_println_simple output");
}

#[test_case]
fn test_println_many() {
    for _ in 0..200 {
        println!("test_println_many output");
    }
}

#[test_case]
fn test_println_output() {
    let s = "Some test string that fits on a single line";
    interrupts::without_interrupts(|| {
        let mut writer = WRITER.lock();
        writeln!(writer, "\n{}", s).expect("writeln failed");
        for (i, c) in s.chars().enumerate() {
            let screen_char = writer.buffer.chars[BUFFER_HEIGHT - 2][i].read();
            assert_eq!(char::from(screen_char.ascii_character), c);
        }
    });
}