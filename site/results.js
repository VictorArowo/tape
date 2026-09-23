// Fills the result sections from JSON the engine writes. While a file is
// marked `placeholder`, its section shows a visible flag so fake numbers
// can't go live unnoticed.

import { barChart, lineChart, scatterChart, tableView } from "./charts.js";
import { colors, fmt } from "./format.js";

function bind(section, values) {
  for (const node of section.querySelectorAll("[data-bind]")) {
    const value = values[node.dataset.bind];
    if (value !== undefined) node.textContent = value;
  }
}

function flag(section, placeholder) {
  const badge = section.querySelector(".placeholder-flag");
  if (badge) badge.hidden = !placeholder;
}

async function load(name) {
  const res = await fetch(`data/results/${name}.json`);
  if (!res.ok) throw new Error(`${name}: ${res.status}`);
  return res.json();
}

async function honestVsNaive() {
  const section = document.querySelector('[data-result="honest-vs-naive"]');
  const data = await load("honest-vs-naive");
  flag(section, data.placeholder);

  const values = {
    strategy: data.strategy.charAt(0).toLowerCase() + data.strategy.slice(1),
    instrument: data.instrument,
    from: fmt.date(data.from),
    to: fmt.date(data.to),
    initialCash: fmt.money(data.initialCash),
  };
  for (const key of ["naive", "tape", "buyHold"]) {
    const s = data.stats[key];
    if (!s) continue;
    Object.assign(values, {
      [`${key}.finalEquity`]: fmt.money(s.finalEquity),
      [`${key}.totalReturn`]: fmt.percent(s.totalReturn),
      [`${key}.sharpe`]: fmt.ratio(s.sharpe),
      [`${key}.maxDrawdown`]: fmt.percent(s.maxDrawdown),
      [`${key}.trades`]: fmt.count(s.trades),
      [`${key}.feesPaid`]: fmt.money(s.feesPaid),
    });
  }
  const tape = data.stats.tape;
  values.verdict = `The strategy's signals are the same in both runs. What changes is the cost of acting on them: ${fmt.money(tape.feesPaid)} in fees across ${fmt.count(tape.trades)} trades.`;
  if (data.stats.buyHold) {
    values.verdict += ` Simply holding for the whole period returned ${fmt.percent(data.stats.buyHold.totalReturn)}.`;
  }
  bind(section, values);
  if (data.ladder?.length) ladder(section, data);

  const host = section.querySelector("#compare-chart");
  lineChart(host, {
    x: data.dates,
    series: [
      { key: "naive", label: "Naive backtest", color: colors.naive, values: data.series.naive },
      { key: "tape", label: "tape", color: colors.tape, values: data.series.tape },
    ],
    yFormat: (v, exact) => (exact ? fmt.moneyExact(v) : fmt.money(v)),
    xFormat: (iso, long) => (long ? fmt.date(iso) : fmt.monthShort(iso)),
    ariaLabel: `Equity over time. Naive backtest ends at ${values["naive.finalEquity"]}; tape ends at ${values["tape.finalEquity"]}.`,
  });
  const monthly = data.dates
    .map((d, i) => [d, i])
    .filter(([d], i, all) => i === all.length - 1 || all[i + 1][0].slice(0, 7) !== d.slice(0, 7));
  tableView(host, {
    summary: "Show month-end equity as a table",
    columns: ["Date", "Naive backtest", "tape"],
    rows: monthly.map(([d, i]) => [fmt.date(d), fmt.money(data.series.naive[i]), fmt.money(data.series.tape[i])]),
  });
}

const LADDER_LABELS = {
  notebook: "Notebook backtest that peeks one bar ahead",
  naive: "Event-driven, but free and instant",
  tape: "+ volume limits: tape",
};

function ladder(section, data) {
  const container = section.querySelector("[data-ladder]");
  const rows = data.ladder.map((step) => ({
    key: step.key,
    label: LADDER_LABELS[step.key] ?? step.label,
    value: step.stats.totalReturn,
    color: step.key === "tape" ? colors.tape : step.key === "naive" ? colors.naive : colors.neutral,
    detail: [
      { label: "fees", value: fmt.money(step.stats.feesPaid) },
      ...(step.stats.trades ? [{ label: "trades", value: fmt.count(step.stats.trades) }] : []),
      ...(step.runHash ? [{ label: "run hash", value: step.runHash }] : []),
    ],
  }));
  const host = container.querySelector("#ladder-chart");
  container.hidden = false;
  const buyHold = data.stats.buyHold;
  barChart(host, {
    rows,
    format: (v) => fmt.percent(v),
    tickFormat: (v) => `${v > 0 ? "+" : v < 0 ? "−" : ""}${Math.round(Math.abs(v) * 100)}%`,
    reference: buyHold ? { value: buyHold.totalReturn, label: `Buy and hold ${fmt.percent(buyHold.totalReturn)}` } : null,
    ariaLabel: `Total return as each assumption is added: ${rows.map((r) => `${r.label} ${fmt.percent(r.value)}`).join("; ")}.`,
  });
  tableView(host, {
    summary: "Show each step as a table",
    columns: ["Step", "Return", "Fees", "Trades", "Run hash"],
    rows: data.ladder.map((step) => [
      LADDER_LABELS[step.key] ?? step.label,
      fmt.percent(step.stats.totalReturn),
      fmt.money(step.stats.feesPaid),
      step.stats.trades ? fmt.count(step.stats.trades) : "",
      step.runHash ?? "",
    ]),
  });

  // Say which step did the damage and which made no difference, from the data.
  const steps = data.ladder.slice(1).map((step, i) => ({
    label: step.label.replace(/^\+\s*/, ""),
    before: data.ladder[i].stats.totalReturn,
    after: step.stats.totalReturn,
  }));
  const biggest = steps.reduce((a, b) => (b.before - b.after > a.before - a.after ? b : a));
  const flat = steps.filter((s) => Math.abs(s.after - s.before) < 0.0005);
  let note = `The biggest single change is adding ${biggest.label}, which takes the result from ${fmt.percent(biggest.before)} to ${fmt.percent(biggest.after)}.`;
  if (flat.length) {
    const names = flat.map((s) => s.label.replace(/\s*\(.*\)$/, ""));
    const list = names.length > 1 ? `${names.slice(0, -1).join(", ")} and ${names.at(-1)}` : names[0];
    note += ` Adding ${list} ${names.length > 1 ? "make" : "makes"} no measurable difference here.`;
  }
  bind(container, { ladderNote: note });
}

async function experiment() {
  const section = document.querySelector('[data-result="experiment"]');
  const data = await load("experiment");
  flag(section, data.placeholder);

  const range = (r) => `${fmt.date(r.from)} to ${fmt.date(r.to)}`;
  bind(section, {
    train: range(data.train),
    test: range(data.test),
    "funnel.proposed": fmt.count(data.funnel.proposed),
    "funnel.compiled": fmt.count(data.funnel.compiled),
    "funnel.passedCritic": fmt.count(data.funnel.passedCritic),
    "funnel.beatBuyHold": fmt.count(data.funnel.beatBuyHold),
  });

  const bh = data.buyHoldOutOfSampleSharpe;
  const host = section.querySelector("#experiment-chart");
  scatterChart(host, {
    points: data.strategies.map((s) => ({
      x: s.inSampleSharpe,
      y: s.outOfSampleSharpe,
      label: s.name,
      group: s.outOfSampleSharpe > bh ? "beat" : "lost",
    })),
    groups: [
      { key: "beat", label: "Beat buy-and-hold on unseen data", color: colors.tape },
      { key: "lost", label: "Didn't", color: colors.naive },
    ],
    xLabel: "Sharpe ratio in training",
    yLabel: "Sharpe ratio in the final exam",
    format: fmt.ratio,
    diagonal: "Backtest told the truth",
    hline: { value: bh, label: "Buy-and-hold" },
    ariaLabel: `Scatter plot of ${data.strategies.length} strategies. ${data.funnel.beatBuyHold} beat buy-and-hold on unseen data.`,
  });
  tableView(host, {
    summary: "Show every strategy as a table",
    columns: ["Strategy", "Sharpe in training", "Sharpe in final exam"],
    rows: data.strategies.map((s) => [s.name, fmt.ratio(s.inSampleSharpe), fmt.ratio(s.outOfSampleSharpe)]),
  });

  const list = section.querySelector('[data-bind-list="examples"]');
  list.replaceChildren(
    ...data.examples.map((e) => {
      const li = document.createElement("li");
      const name = document.createElement("h4");
      name.textContent = e.name;
      const idea = document.createElement("p");
      idea.textContent = e.hypothesis;
      const scores = document.createElement("p");
      scores.className = "notebook-scores";
      scores.textContent = `Sharpe ${fmt.ratio(e.inSampleSharpe)} in training, ${fmt.ratio(e.outOfSampleSharpe)} in the final exam`;
      const critic = document.createElement("blockquote");
      critic.textContent = e.critic;
      li.append(name, idea, scores, critic);
      return li;
    }),
  );
}

for (const task of [honestVsNaive, experiment]) {
  task().catch((err) => console.error("Failed to load results:", err));
}
