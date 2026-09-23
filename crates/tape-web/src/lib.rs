//! tape in the browser.
//!
//! Built for `wasm32-unknown-unknown`, this crate exposes four plain
//! functions that `tape-engine.js` drives. There's no binding generator:
//! data goes in and out through two byte buffers the module owns.
//!
//! - `tape_input(len) -> ptr`: make room for `len` bytes and return where
//!   to write them.
//! - `tape_load(len) -> count`: decode a month of bars from the input
//!   buffer; returns the number of bars, or, if the data is bad, minus the
//!   length of a JSON error in the output buffer.
//! - `tape_run(len) -> len`: run the backtest described by the JSON in the
//!   input buffer; returns the length of the JSON result in the output
//!   buffer (a result, or `{"error": ...}`).
//! - `tape_output() -> ptr`: where that result is.

pub mod demo;
pub mod format;

use std::cell::RefCell;

use format::Dataset;

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static DATASET: RefCell<Option<Dataset>> = const { RefCell::new(None) };
}

fn set_output(text: String) -> u32 {
    OUTPUT.with(|out| {
        let mut out = out.borrow_mut();
        *out = text.into_bytes();
        out.len() as u32
    })
}

fn with_input<T>(len: u32, f: impl FnOnce(&[u8]) -> T) -> T {
    INPUT.with(|input| {
        let input = input.borrow();
        f(&input[..(len as usize).min(input.len())])
    })
}

/// Makes room for `len` input bytes and returns their address.
pub fn input(len: u32) -> u32 {
    INPUT.with(|input| {
        let mut input = input.borrow_mut();
        input.clear();
        input.resize(len as usize, 0);
        input.as_mut_ptr() as usize as u32
    })
}

/// Decodes the bars in the input buffer and keeps them for later runs.
pub fn load(len: u32) -> i32 {
    match with_input(len, format::decode) {
        Ok(dataset) => {
            let count = dataset.bars.len() as i32;
            DATASET.with(|d| *d.borrow_mut() = Some(dataset));
            count
        }
        Err(message) => -(set_output(serde_json::json!({ "error": message }).to_string()) as i32),
    }
}

/// Runs the backtest described by the JSON in the input buffer.
pub fn run(len: u32) -> u32 {
    let params = with_input(len, |bytes| String::from_utf8_lossy(bytes).into_owned());
    let result = DATASET.with(|d| demo::run_json(d.borrow().as_ref(), &params));
    set_output(result)
}

/// The address of the last result.
pub fn output() -> u32 {
    OUTPUT.with(|out| out.borrow().as_ptr() as usize as u32)
}

// Exporting unmangled functions is the one thing here that needs `unsafe`
// (it's an attribute, not unsafe code), and only in the WebAssembly build.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod exports {
    #[unsafe(no_mangle)]
    pub extern "C" fn tape_input(len: u32) -> u32 {
        super::input(len)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn tape_load(len: u32) -> i32 {
        super::load(len)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn tape_run(len: u32) -> u32 {
        super::run(len)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn tape_output() -> u32 {
        super::output()
    }
}
