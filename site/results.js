// Fills the result sections from JSON the engine writes. While a file is
// marked `placeholder`, its section shows a visible flag so fake numbers
// can't go live unnoticed.

import { lineChart, scatterChart, tableView } from "./charts.js";
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
  for (const key of ["naive", "tape"]) {
    const s = data.stats[key];
    Object.assign(values, {
      [`${key}.finalEquity`]: fmt.money(s.finalEquity),
      [`${key}.totalReturn`]: fmt.percent(s.totalReturn),
      [`${key}.sharpe`]: fmt.ratio(s.sharpe),
      [`${key}.maxDrawdown`]: fmt.percent(s.maxDrawdown),
      [`${key}.trades`]: fmt.count(s.trades),
      [`${key}.feesPaid`]: fmt.money(s.feesPaid),
    });
  }
  bind(section, values);

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
