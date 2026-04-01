use std::{ffi::c_void, ptr};

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyResult {
    Success = 0,
    OutOfMemory = -1,
    InvalidValue = -2,
    OutOfSpace = -3,
    NoValue = -4,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyTerminalOptions {
    pub cols: u16,
    pub rows: u16,
    pub max_scrollback: usize,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub enum GhosttyFormatterFormat {
    Plain = 0,
    Vt = 1,
    Html = 2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyFormatterScreenExtra {
    pub size: usize,
    pub cursor: bool,
    pub style: bool,
    pub hyperlink: bool,
    pub protection: bool,
    pub kitty_keyboard: bool,
    pub charsets: bool,
}

impl GhosttyFormatterScreenExtra {
    pub fn init() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            cursor: false,
            style: false,
            hyperlink: false,
            protection: false,
            kitty_keyboard: false,
            charsets: false,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyFormatterTerminalExtra {
    pub size: usize,
    pub palette: bool,
    pub modes: bool,
    pub scrolling_region: bool,
    pub tabstops: bool,
    pub pwd: bool,
    pub keyboard: bool,
    pub screen: GhosttyFormatterScreenExtra,
}

impl GhosttyFormatterTerminalExtra {
    pub fn init() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            palette: false,
            modes: false,
            scrolling_region: false,
            tabstops: false,
            pwd: false,
            keyboard: false,
            screen: GhosttyFormatterScreenExtra::init(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyFormatterTerminalOptions {
    pub size: usize,
    pub emit: GhosttyFormatterFormat,
    pub unwrap: bool,
    pub trim: bool,
    pub extra: GhosttyFormatterTerminalExtra,
}

impl GhosttyFormatterTerminalOptions {
    pub fn init() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            emit: GhosttyFormatterFormat::Vt,
            unwrap: false,
            trim: false,
            extra: GhosttyFormatterTerminalExtra::init(),
        }
    }
}

pub enum GhosttyTerminalOpaque {}
pub enum GhosttyFormatterOpaque {}

pub type GhosttyTerminal = *mut GhosttyTerminalOpaque;
pub type GhosttyFormatter = *mut GhosttyFormatterOpaque;

#[link(name = "ghostty-vt")]
unsafe extern "C" {
    fn ghostty_terminal_new(allocator: *const c_void, terminal: *mut GhosttyTerminal, options: GhosttyTerminalOptions) -> GhosttyResult;
    fn ghostty_terminal_free(terminal: GhosttyTerminal);
    fn ghostty_terminal_resize(terminal: GhosttyTerminal, cols: u16, rows: u16, cell_width_px: u32, cell_height_px: u32) -> GhosttyResult;
    fn ghostty_terminal_vt_write(terminal: GhosttyTerminal, data: *const u8, len: usize);

    fn ghostty_formatter_terminal_new(
        allocator: *const c_void,
        formatter: *mut GhosttyFormatter,
        terminal: GhosttyTerminal,
        options: GhosttyFormatterTerminalOptions,
    ) -> GhosttyResult;
    fn ghostty_formatter_format_buf(formatter: GhosttyFormatter, buf: *mut u8, buf_len: usize, out_written: *mut usize) -> GhosttyResult;
    fn ghostty_formatter_free(formatter: GhosttyFormatter);
}

pub struct TerminalHandle {
    raw: GhosttyTerminal,
}

impl TerminalHandle {
    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Result<Self, String> {
        let mut raw = ptr::null_mut();
        let result = unsafe { ghostty_terminal_new(ptr::null(), &mut raw, GhosttyTerminalOptions { cols, rows, max_scrollback }) };
        check_result(result, "ghostty_terminal_new")?;
        Ok(Self { raw })
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        let result = unsafe { ghostty_terminal_resize(self.raw, cols, rows, 1, 1) };
        check_result(result, "ghostty_terminal_resize")
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        unsafe { ghostty_terminal_vt_write(self.raw, bytes.as_ptr(), bytes.len()) };
    }

    pub fn raw(&self) -> GhosttyTerminal {
        self.raw
    }
}

impl Drop for TerminalHandle {
    fn drop(&mut self) {
        unsafe { ghostty_terminal_free(self.raw) };
    }
}

pub fn format_terminal_alloc(terminal: GhosttyTerminal, options: GhosttyFormatterTerminalOptions) -> Result<Vec<u8>, String> {
    let mut formatter = ptr::null_mut();
    let result = unsafe { ghostty_formatter_terminal_new(ptr::null(), &mut formatter, terminal, options) };
    check_result(result, "ghostty_formatter_terminal_new")?;

    let bytes = format_terminal_into_owned_buffer(formatter);
    unsafe { ghostty_formatter_free(formatter) };
    bytes
}

fn format_terminal_into_owned_buffer(formatter: GhosttyFormatter) -> Result<Vec<u8>, String> {
    let mut required = 0usize;
    let result = unsafe { ghostty_formatter_format_buf(formatter, ptr::null_mut(), 0, &mut required) };
    match result {
        GhosttyResult::OutOfSpace => {}
        GhosttyResult::Success => return Ok(Vec::new()),
        other => return check_result(other, "ghostty_formatter_format_buf").map(|_| Vec::new()),
    }

    let mut bytes = vec![0u8; required];
    let mut written = 0usize;
    let result = unsafe { ghostty_formatter_format_buf(formatter, bytes.as_mut_ptr(), bytes.len(), &mut written) };
    check_result(result, "ghostty_formatter_format_buf")?;
    bytes.truncate(written);
    Ok(bytes)
}

fn check_result(result: GhosttyResult, op: &str) -> Result<(), String> {
    match result {
        GhosttyResult::Success => Ok(()),
        GhosttyResult::OutOfMemory => Err(format!("{op} failed: out of memory")),
        GhosttyResult::InvalidValue => Err(format!("{op} failed: invalid value")),
        GhosttyResult::OutOfSpace => Err(format!("{op} failed: out of space")),
        GhosttyResult::NoValue => Err(format!("{op} failed: no value")),
    }
}
