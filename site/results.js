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

// Placeholder data and small pilot runs both get a visible badge.
function flag(section, data) {
  const badge = section.querySelector(".placeholder-flag");
  if (!badge) return;
  const text = data.placeholder ? "Placeholder data" : data.pilot ? `Pilot run: ${data.funnel?.proposed ?? "a few"} ideas` : null;
  badge.hidden = !text;
  if (text) badge.textContent = text;
}

function correlation(points) {
  const n = points.length;
  if (n < 3) return null;
  const mx = points.reduce((a, p) => a + p.x, 0) / n;
  const my = points.reduce((a, p) => a + p.y, 0) / n;
  let sxy = 0, sxx = 0, syy = 0;
  for (const p of points) {
    sxy += (p.x - mx) * (p.y - my);
    sxx += (p.x - mx) ** 2;
    syy += (p.y - my) ** 2;
  }
  return sxx && syy ? sxy / Math.sqrt(sxx * syy) : null;
}

async function load(name) {
  const res = await fetch(`data/results/${name}.json`);
  if (!res.ok) throw new Error(`${name}: ${res.status}`);
  return res.json();
}

async function honestVsNaive() {
  const section = document.querySelector('[data-result="honest-vs-naive"]');
  const data = await load("honest-vs-naive");
  flag(section, data);

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

// The deflated Sharpe probability is the verdict; the thresholds explain it.
function luckNote(ds) {
  const tries = `${fmt.count(ds.trials)} tries`;
  if (ds.probability == null) return "There isn't enough data to judge: the best strategy has fewer than three days of returns.";
  if (ds.trials === 1) {
    return ds.probability >= 0.5
      ? "With only one strategy tried, this reduces to whether its Sharpe ratio is positive at all. It is, but that says little on its own."
      : "With only one strategy tried, this reduces to whether its Sharpe ratio is positive at all. It isn't.";
  }
  if (ds.probability >= 0.95) return `Even after ${tries}, the best strategy's edge is unlikely to be luck. The final exam is the real test.`;
  if (ds.probability >= 0.5) return `The best strategy did better than luck would predict after ${tries}, but not by a convincing margin.`;
  return `After ${tries}, the best strategy's training result is about what luck alone would produce.`;
}

async function experiment() {
  const section = document.querySelector('[data-result="experiment"]');
  const data = await load("experiment");
  flag(section, data);

  const range = (r) => `${fmt.date(r.from)} to ${fmt.date(r.to)}`;
  const f = data.funnel;
  const values = {
    train: range(data.train),
    test: range(data.test),
    "funnel.proposed": fmt.count(f.proposed),
    "funnel.compiled": fmt.count(f.compiled),
    "funnel.ran": fmt.count(f.ran ?? f.compiled),
    "funnel.passedCritic": fmt.count(f.passedCritic),
    "funnel.beatBuyHold": fmt.count(f.beatBuyHold),
  };

  // Plot everything that ran if we have it; the critic may pass very little.
  const bh = data.buyHoldOutOfSampleSharpe;
  const pool = data.allStrategies ?? data.strategies.map((s) => ({ ...s, status: "passed" }));
  const plottable = pool.filter((s) => s.inSampleSharpe != null && s.outOfSampleSharpe != null);
  const unplotted = pool.filter((s) => s.status !== "runtime_failed").length - plottable.length;
  const groupOf = (s) => (s.status !== "passed" ? "rejected" : s.outOfSampleSharpe > bh ? "beat" : "lost");
  const groups = [
    { key: "beat", label: "Passed, and beat buy-and-hold on unseen data", color: colors.tape },
    { key: "lost", label: "Passed, but didn't", color: colors.naive },
    { key: "rejected", label: "Rejected by the critic", color: colors.rejected, hollow: true },
  ].filter((g) => plottable.some((s) => groupOf(s) === g.key));

  const r = correlation(plottable.map((s) => ({ x: s.inSampleSharpe, y: s.outOfSampleSharpe })));
  values.scatterSub =
    `Each dot is a strategy that ran. If backtests told the truth, dots would sit on the diagonal.` +
    (r != null ? ` The correlation between training and final-exam scores is ${fmt.ratio(r)}.` : "") +
    (unplotted > 0 ? ` Not shown: ${fmt.count(unplotted)} ${unplotted === 1 ? "strategy" : "strategies"} without both scores.` : "");

  const host = section.querySelector("#experiment-chart");
  const { pinned } = scatterChart(host, {
    points: plottable.map((s) => ({ x: s.inSampleSharpe, y: s.outOfSampleSharpe, label: s.name, group: groupOf(s) })),
    groups,
    xLabel: "Sharpe ratio in training",
    yLabel: "Sharpe ratio in the final exam",
    format: fmt.ratio,
    diagonal: "Backtest told the truth",
    hline: { value: bh, label: ["Buy and hold,", fmt.period(data.test.from, data.test.to)] },
    vline:
      data.buyHoldInSampleSharpe != null
        ? { value: data.buyHoldInSampleSharpe, label: ["Buy and hold,", fmt.period(data.train.from, data.train.to)] }
        : null,
    ariaLabel: `Scatter plot of ${plottable.length} strategies. ${f.beatBuyHold} passed the critic and beat buy-and-hold on unseen data.`,
  });
  if (pinned > 0) {
    values.scatterSub += ` ${pinned === 1 ? "One extreme result is" : `${fmt.count(pinned)} extreme results are`} pinned to the edge; hover for the real value.`;
  }
  const statusLabel = { passed: "Passed", rejected: "Rejected", runtime_failed: "Crashed" };
  tableView(host, {
    summary: "Show every strategy as a table",
    columns: ["Strategy", "Critic", "Sharpe in training", "Sharpe in final exam"],
    rows: pool.map((s) => [s.name, statusLabel[s.status] ?? "", fmt.ratio(s.inSampleSharpe), fmt.ratio(s.outOfSampleSharpe)]),
  });

  const ds = data.deflatedSharpe;
  if (ds) {
    section.querySelector("[data-luck]").hidden = false;
    Object.assign(values, {
      "luck.best": fmt.ratio(ds.bestInSampleSharpe),
      "luck.bestLabel": `best Sharpe ratio in training (${pool.find((s) => s.id === ds.best)?.name ?? ds.best})`,
      "luck.threshold": fmt.ratio(ds.luckThresholdSharpe),
      "luck.thresholdLabel": `the best of ${fmt.count(ds.trials)} skill-less tries, by luck alone`,
      "luck.probability": ds.probability == null ? "Not enough data" : `${Math.round(ds.probability * 100)}%`,
      "luck.note": luckNote(ds),
    });
  }

  const meta = [];
  if (data.model) meta.push(`Agents ran on ${data.model}.`);
  if (data.spentUsd != null) meta.push(`The run cost ${fmt.moneyExact(data.spentUsd)} in API calls.`);
  if (data.stopped) meta.push(`It stopped because: ${data.stopped.charAt(0).toLowerCase()}${data.stopped.slice(1)}`);
  values.runMeta = meta.join(" ");
  bind(section, values);

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
