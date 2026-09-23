//! What the three agents are told. The system prompts are fixed for a run,
//! so the provider can cache them; everything that changes goes in the
//! user message.

use serde_json::{Value, json};

use crate::model::Prompt;

pub struct MarketBrief {
    pub symbol: String,
    pub train_from: String,
    pub train_to: String,
    pub buy_hold_return: f64,
    pub buy_hold_sharpe: Option<f64>,
    pub initial_cash: f64,
}

const COSTS: &str = "\
Execution is simulated realistically: every order is filled at the next bar's open, never the bar \
that triggered it; each fill pays a 0.1% fee plus 0.02% slippage (so a round trip costs about 0.24%); \
orders take 100 ms to reach the exchange; fills are capped at 10% of a bar's volume; resting limit \
orders only fill if the market trades through their price. The account is spot only: it can't sell \
more than it holds or borrow. Strategies that trade often usually lose to these costs.";

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub fn researcher(brief: &MarketBrief, history: &str) -> Prompt {
    let system = format!(
        "You are a quantitative researcher looking for a trading strategy with a real, repeatable edge.\n\n\
         Market: {symbol} spot, one-minute bars (open, high, low, close, volume) from {from} to {to}. \
         Buying and holding over that period returned {bh_return:+.1}% (daily Sharpe ratio {bh_sharpe}). \
         Starting cash is {cash:.0} USDT.\n\n{COSTS}\n\n\
         Propose ONE strategy idea at a time. It must be long-only or flat, simple enough to implement \
         in about a hundred lines, and built only from the bars it has seen so far (prices, volumes and \
         the time of day or day of week from bar timestamps). Prefer ideas with an economic reason to \
         work over pattern-matching. Make each idea genuinely different from the ones already tried, \
         and learn from why earlier ideas failed.",
        symbol = brief.symbol,
        from = brief.train_from,
        to = brief.train_to,
        bh_return = brief.buy_hold_return * 100.0,
        bh_sharpe = brief
            .buy_hold_sharpe
            .map_or("n/a".into(), |s| format!("{s:.2}")),
        cash = brief.initial_cash,
    );
    let user = if history.is_empty() {
        "No ideas have been tried yet. Propose the first one.".to_owned()
    } else {
        format!("Ideas tried so far, with what happened:\n{history}\n\nPropose the next idea.")
    };
    Prompt {
        system,
        user,
        schema_name: "hypothesis",
        schema: schema(
            json!({
                "name": {"type": "string", "description": "Short name, at most six words"},
                "hypothesis": {"type": "string", "description": "Why this should work, in one or two sentences"},
                "rules": {"type": "string", "description": "Exact entry, exit and sizing rules, with any parameter values"},
            }),
            &["name", "hypothesis", "rules"],
        ),
    }
}

const SDK: &str = r#"Write the strategy as a single Rust file (src/lib.rs, edition 2024) against the `tape_guest` SDK. It is compiled to WebAssembly and runs in a sandbox with no clock, files, network or randomness. Only `std` (e.g. std::collections::VecDeque) and `tape_guest` are available.

The SDK (everything is re-exported from `tape_guest`):

pub trait Strategy {
    fn name(&self) -> String;
    fn on_start(&mut self, _ctx: &mut Ctx) {}
    fn on_bar(&mut self, _ctx: &mut Ctx, _bar: &Bar) {}      // called once per one-minute bar, after it closes
    fn on_fill(&mut self, _ctx: &mut Ctx, _fill: &Fill) {}
    fn on_order(&mut self, _ctx: &mut Ctx, _update: &OrderUpdate) {}
    fn on_finish(&mut self, _ctx: &mut Ctx) {}
}
export_strategy!(expr);   // exactly once, at the top level: registers the strategy value

pub struct Bar { pub open_ts: Timestamp, pub close_ts: Timestamp, pub instrument: InstrumentId,
                 pub open: Price, pub high: Price, pub low: Price, pub close: Price, pub volume: Qty }
pub struct Fill { pub order_id: OrderId, pub ts: Timestamp, pub side: Side, pub price: Price, pub qty: Qty, pub fee: Money, .. }

Ctx (the only way to see the account or trade):
  ctx.now() -> Timestamp            ctx.cash() -> Money          ctx.equity() -> Money
  ctx.position(InstrumentId) -> Qty ctx.last_price(InstrumentId) -> Option<Price>
  ctx.open_orders() -> usize        // orders sent and not yet finished
  ctx.affordable_qty(instrument, price, cash, margin_ppm) -> Qty   // largest buy after fees; use margin_ppm = 5_000
  ctx.submit(OrderRequest) -> OrderId     ctx.cancel(OrderId)     ctx.log(&str)

Orders:
  OrderRequest::market(instrument, Side::Buy | Side::Sell, qty)        // immediate-or-cancel
  OrderRequest::limit(instrument, side, qty, price)                    // rests until filled or cancelled
  OrderRequest::stop(instrument, side, qty, trigger)                   // becomes a market order at trigger
  .with_time_in_force(TimeInForce::Gtc | TimeInForce::Ioc)
The traded instrument is InstrumentId(1). Quantities must be positive; sell at most ctx.position(..).

Numbers are exact fixed-point integers with 8 decimal places (1.0 is raw 100_000_000):
  Price, Qty, Money: ::from_raw(i64), .raw() -> i64, ::from_int(i64) -> Option<Self>, .to_f64(),
  +, -, comparisons, .is_zero(), .is_positive(), .abs(), .mul_ratio(num, den) (exact scaling),
  Price * Qty -> Money. Use f64 (via .to_f64()) for indicators; convert back with Price::from_raw((x * 1e8) as i64).
Timestamp: .as_nanos() -> i64 (nanoseconds since 1970). TimeDelta::from_mins/hours/days(n).as_nanos().
  Minute of day: (bar.close_ts.as_nanos() / TimeDelta::from_mins(1).as_nanos()) % 1440.
  Day of week, 0 = Thursday: (bar.close_ts.as_nanos() / TimeDelta::from_days(1).as_nanos()) % 7.

Rules the build enforces (the file is rejected otherwise):
- No `unsafe`, `include_str!`/`include_bytes!`/`include!`, `env!`, `mod x;`, crate attributes other than #![allow(..)], std::fs/net/process/env/time/thread.
- No hard-coded dates: no integer literals with 10 or more digits, no Timestamp::from_ymd, no .date(). The strategy must work on any period.
- Before submitting, check ctx.open_orders() == 0 (or track your own orders) so you don't pile up duplicate orders.
- Use ctx.affordable_qty for buys so fees can't overdraw the account.

Example, a complete moving-average crossover:
"#;

pub fn coder_system() -> String {
    format!(
        "You are an expert Rust developer who writes trading strategies for a backtesting sandbox.\n\n{SDK}\n```rust\n{}\n```\n\n\
         Return the complete file. Keep it simple and correct; it must compile on the first try.",
        include_str!("../../../guests/sma-cross/src/lib.rs")
    )
}

fn code_schema() -> Value {
    schema(
        json!({
            "code": {"type": "string", "description": "The complete src/lib.rs"},
            "notes": {"type": "string", "description": "Anything the reviewer should know, briefly"},
        }),
        &["code", "notes"],
    )
}

pub fn coder(system: &str, idea: &Value) -> Prompt {
    Prompt {
        system: system.to_owned(),
        user: format!(
            "Implement this strategy exactly as described:\n{}",
            serde_json::to_string_pretty(idea).unwrap_or_default()
        ),
        schema_name: "strategy_code",
        schema: code_schema(),
    }
}

pub fn repair(system: &str, idea: &Value, code: &str, error: &str) -> Prompt {
    Prompt {
        system: system.to_owned(),
        user: format!(
            "Implement this strategy:\n{}\n\nYour previous file failed:\n{error}\n\nPrevious file:\n```rust\n{code}\n```\n\n\
             Return the complete corrected file.",
            serde_json::to_string_pretty(idea).unwrap_or_default()
        ),
        schema_name: "strategy_code",
        schema: code_schema(),
    }
}

pub fn critic(evidence: &str) -> Prompt {
    let system = format!(
        "You are a skeptical reviewer on a trading desk. Most backtested strategies that look good are \
         overfit, lucky, or only work because of one unusual stretch of data, and your job is to stop \
         those before real money is committed. You never see future data; judge only the evidence given.\n\n\
         {COSTS}\n\n\
         Pass a strategy only if all of these hold: it is profitable after costs; it performs acceptably \
         in BOTH halves of the period, not just one; it makes enough trades for the result to mean \
         something (a handful of trades proves nothing); it beats simply holding, or offers clearly lower \
         risk for similar return; and its rules are simple, with no suspiciously specific parameters. \
         Reject anything else, and say plainly why."
    );
    Prompt {
        system,
        user: evidence.to_owned(),
        schema_name: "critique",
        schema: schema(
            json!({
                "verdict": {"type": "string", "enum": ["pass", "reject"]},
                "reason": {"type": "string", "description": "The deciding reason, in one or two sentences"},
                "concerns": {"type": "array", "items": {"type": "string"}},
            }),
            &["verdict", "reason", "concerns"],
        ),
    }
}
