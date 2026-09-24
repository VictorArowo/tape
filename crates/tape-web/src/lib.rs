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
//!
//! A run with `"strategy": "custom"` hands its decisions to JavaScript: the
//! module imports `on_start`, `on_bar`, `on_fill`, `on_order` and
//! `on_finish` from `strategy`, and while one of them runs, the code can
//! call `tape_now`, `tape_cash`, `tape_equity`, `tape_position`,
//! `tape_price`, `tape_open_orders`, `tape_max_buy`, `tape_submit`,
//! `tape_cancel` and `tape_log` (see [`custom`]). Each import returns 0, or
//! if the code threw, the length of the error message it wrote to the input
//! buffer.

pub mod custom;
pub mod demo;
pub mod format;

use std::cell::RefCell;

use format::Dataset;
use tape_engine::Strategy;

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

/// The strategy for a `"custom"` run: the page's JavaScript, which only
/// the browser build can call.
pub fn custom_strategy() -> Result<Box<dyn Strategy>, String> {
    #[cfg(target_arch = "wasm32")]
    {
        Ok(Box::new(custom::JsStrategy::new(js::Imports)))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Err("custom strategies run only in the browser".into())
    }
}

/// The JavaScript strategy's callbacks, imported from the page.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod js {
    use tape_core::{Bar, Fill, abi};
    use tape_engine::OrderUpdate;

    use crate::custom::{Callbacks, update_args};

    #[link(wasm_import_module = "strategy")]
    unsafe extern "C" {
        fn on_start() -> u32;
        fn on_bar(time_ms: f64, open: f64, high: f64, low: f64, close: f64, volume: f64) -> u32;
        fn on_fill(
            id: f64,
            time_ms: f64,
            side: i32,
            price: f64,
            qty: f64,
            fee: f64,
            liquidity: i32,
        ) -> u32;
        fn on_order(id: f64, time_ms: f64, update: i32, reason: i32, remaining: f64) -> u32;
        fn on_finish() -> u32;
    }

    fn check(len: u32) -> Result<(), String> {
        match len {
            0 => Ok(()),
            len => Err(super::with_input(len, |bytes| {
                String::from_utf8_lossy(bytes).into_owned()
            })),
        }
    }

    fn ms(ts: tape_core::Timestamp) -> f64 {
        ts.as_nanos() as f64 / 1e6
    }

    pub struct Imports;

    // Each import is a plain function of numbers that the page provides;
    // calling it is only unsafe in that Rust can't check what's on the
    // other side.
    impl Callbacks for Imports {
        fn on_start(&mut self) -> Result<(), String> {
            check(unsafe { on_start() })
        }

        fn on_bar(&mut self, bar: &Bar) -> Result<(), String> {
            check(unsafe {
                on_bar(
                    ms(bar.close_ts),
                    bar.open.to_f64(),
                    bar.high.to_f64(),
                    bar.low.to_f64(),
                    bar.close.to_f64(),
                    bar.volume.to_f64(),
                )
            })
        }

        fn on_fill(&mut self, fill: &Fill) -> Result<(), String> {
            check(unsafe {
                on_fill(
                    fill.order_id.0 as f64,
                    ms(fill.ts),
                    abi::side_code(fill.side),
                    fill.price.to_f64(),
                    fill.qty.to_f64(),
                    fill.fee.to_f64(),
                    abi::liquidity_code(fill.liquidity),
                )
            })
        }

        fn on_order(&mut self, update: &OrderUpdate) -> Result<(), String> {
            let (id, time_ms, code, reason, remaining) = update_args(update);
            check(unsafe { on_order(id, time_ms, code, reason, remaining) })
        }

        fn on_finish(&mut self) -> Result<(), String> {
            check(unsafe { on_finish() })
        }
    }
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

    use crate::custom;

    #[unsafe(no_mangle)]
    pub extern "C" fn tape_now() -> f64 {
        custom::now()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn tape_cash() -> f64 {
        custom::cash()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn tape_equity() -> f64 {
        custom::equity()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn tape_position() -> f64 {
        custom::position()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn tape_price() -> f64 {
        custom::price()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn tape_open_orders() -> u32 {
        custom::open_orders()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn tape_max_buy(price: f64) -> f64 {
        custom::max_buy(price)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn tape_submit(side: i32, kind: i32, qty: f64, price: f64, tif: i32) -> f64 {
        custom::submit(side, kind, qty, price, tif)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn tape_cancel(id: f64) -> f64 {
        custom::cancel(id)
    }

    /// Logs the text in the input buffer.
    #[unsafe(no_mangle)]
    pub extern "C" fn tape_log(len: u32) {
        super::with_input(len, |bytes| custom::log(&String::from_utf8_lossy(bytes)));
    }
}
