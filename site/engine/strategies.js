// Starting points for custom strategies, for the page's editor. Each one
// runs as-is with engine.run({ strategy: "custom", code, ... }).

const crossover = `// Moving-average crossover. Hold bitcoin while the average close of the
// last 20 minutes is above the average of the last 100; sell when it
// drops below.
//
// onBar(bar, ctx) runs after each one-minute bar closes, and only ever
// sees bars up to that one.
//   bar: time, open, high, low, close, volume
//   ctx: cash, position, equity, price, openOrders, time,
//        buy(qty, { limit, stop }), sell(...), cancel(id), maxBuy(price), log(...)

const FAST = 20;
const SLOW = 100;
const closes = [];

function onBar(bar, ctx) {
  closes.push(bar.close);
  if (closes.length > SLOW) closes.shift();
  if (closes.length < SLOW || ctx.openOrders > 0) return;

  const fast = average(closes.slice(-FAST));
  const slow = average(closes);
  if (fast > slow && ctx.position === 0) {
    ctx.buy(ctx.maxBuy());
  } else if (fast < slow && ctx.position > 0) {
    ctx.sell(ctx.position);
  }
}

function average(values) {
  return values.reduce((sum, v) => sum + v, 0) / values.length;
}
`;

const rsi = `// RSI reversal. Buy when the 14-bar relative strength index drops below
// 25 (sold off hard), sell when it climbs back above 55.

const PERIOD = 14;
let previous = null;
let gain = 0;
let loss = 0;
let bars = 0;

function onBar(bar, ctx) {
  if (previous !== null) {
    const change = bar.close - previous;
    // Wilder's smoothing.
    gain = (gain * (PERIOD - 1) + Math.max(change, 0)) / PERIOD;
    loss = (loss * (PERIOD - 1) + Math.max(-change, 0)) / PERIOD;
    bars += 1;
  }
  previous = bar.close;
  if (bars < PERIOD || ctx.openOrders > 0) return;

  const rsi = loss === 0 ? 100 : 100 - 100 / (1 + gain / loss);
  if (rsi < 25 && ctx.position === 0) {
    ctx.buy(ctx.maxBuy());
  } else if (rsi > 55 && ctx.position > 0) {
    ctx.sell(ctx.position);
  }
}
`;

const dip = `// Buy the dip with limit orders. Keep a buy order resting 1% below the
// last hour's high, then offer the bitcoin back 1% above what it cost. If
// the price falls 2% below cost instead, give up and sell at market.
//
// Limit orders wait for the price to come to them, pay the lower maker
// fee, and only fill if the market trades through the price.

const HOUR = 60;
const highs = [];
let buy = null;   // the resting buy: { id, price, moving }
let sell = null;  // the resting sell: { id, cutting }
let cost = 0;     // average price paid for what we hold
let bought = 0;

function onBar(bar, ctx) {
  highs.push(bar.high);
  if (highs.length > HOUR) highs.shift();
  if (highs.length < HOUR) return;
  const target = Math.max(...highs) * 0.99;

  if (ctx.openOrders > 0) {
    // Follow the market: move the buy if the target has drifted 0.25%.
    if (buy && !buy.moving && Math.abs(target / buy.price - 1) > 0.0025) {
      ctx.cancel(buy.id);
      buy.moving = true;
    }
    // Cut the loss: pull the sell, and sell at market once it's gone.
    if (sell && !sell.cutting && bar.close < cost * 0.98) {
      ctx.cancel(sell.id);
      sell.cutting = true;
    }
    return;
  }

  if (ctx.position > 0) {
    if (sell?.cutting) {
      ctx.sell(ctx.position);
      sell = null;
    } else {
      sell = { id: ctx.sell(ctx.position, { limit: cost * 1.01 }), cutting: false };
    }
    return;
  }

  const qty = ctx.maxBuy(target);
  if (qty > 0) buy = { id: ctx.buy(qty, { limit: target }), price: target, moving: false };
}

function onFill(fill, ctx) {
  if (fill.side === "buy") {
    cost = (cost * bought + fill.price * fill.qty) / (bought + fill.qty);
    bought += fill.qty;
  } else if (ctx.position === 0) {
    cost = 0;
    bought = 0;
  }
}

function onOrder(update, ctx) {
  if (update.status === "accepted") return;
  if (buy && update.id === buy.id) buy = null;
  // A sell being cut stays until it's gone, unless it filled first.
  if (sell && update.id === sell.id && (update.status === "filled" || !sell.cutting)) sell = null;
  if (update.status === "rejected") ctx.log("order", update.id, "rejected:", update.reason);
}
`;

export const examples = [
  { id: "crossover", name: "Moving-average crossover", code: crossover },
  { id: "rsi", name: "RSI reversal", code: rsi },
  { id: "dip", name: "Buy the dip with limit orders", code: dip },
];
