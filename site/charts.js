// Small SVG charts for the site: a multi-series line chart with a crosshair,
// and a scatter plot. Both redraw on resize, show a tooltip on hover and
// keyboard focus, and never color text with a series color.

const NS = "http://www.w3.org/2000/svg";

function el(name, attrs = {}, parent) {
  const node = document.createElementNS(NS, name);
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
  if (parent) parent.appendChild(node);
  return node;
}

function niceTicks(min, max, count = 5) {
  const span = max - min || Math.abs(max) || 1;
  const step0 = span / count;
  const mag = 10 ** Math.floor(Math.log10(step0));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => span / s <= count) ?? 10 * mag;
  const lo = Math.floor(min / step) * step;
  const hi = Math.ceil(max / step) * step;
  const ticks = [];
  for (let v = lo; v <= hi + step / 2; v += step) ticks.push(Math.round(v / step) * step);
  return ticks;
}

function makeTooltip(host) {
  const tip = document.createElement("div");
  tip.className = "chart-tip";
  tip.hidden = true;
  host.appendChild(tip);
  return {
    show(x, y, title, rows) {
      tip.replaceChildren();
      const t = document.createElement("div");
      t.className = "chart-tip-title";
      t.textContent = title;
      tip.appendChild(t);
      for (const row of rows) {
        const r = document.createElement("div");
        r.className = "chart-tip-row";
        const key = document.createElement("span");
        key.className = "chart-tip-key";
        key.style.background = row.color;
        const value = document.createElement("strong");
        value.textContent = row.value;
        const label = document.createElement("span");
        label.className = "chart-tip-label";
        label.textContent = row.label;
        r.append(key, value, label);
        tip.appendChild(r);
      }
      tip.hidden = false;
      const hostRect = host.getBoundingClientRect();
      const w = tip.offsetWidth;
      const left = Math.min(Math.max(x + 14, 0), hostRect.width - w);
      tip.style.left = `${x + 14 + w > hostRect.width ? x - w - 14 : left}px`;
      tip.style.top = `${Math.max(0, y - 10)}px`;
    },
    hide() {
      tip.hidden = true;
    },
  };
}

function legend(host, series, shape) {
  const ul = document.createElement("ul");
  ul.className = "chart-legend";
  for (const s of series) {
    const li = document.createElement("li");
    const key = document.createElement("span");
    key.className = `chart-legend-key ${shape}`;
    key.style.background = s.color;
    const label = document.createElement("span");
    label.textContent = s.label;
    li.append(key, label);
    ul.appendChild(li);
  }
  host.appendChild(ul);
}

/**
 * Line chart. `x` is an array of labels (dates), each series has `values`
 * aligned with it. One y-axis only.
 */
export function lineChart(host, { x, series, yFormat, xFormat, height = 320, ariaLabel }) {
  host.classList.add("chart");
  host.replaceChildren();
  if (series.length > 1) legend(host, series, "line");
  const plot = document.createElement("div");
  plot.className = "chart-plot";
  host.appendChild(plot);
  const tip = makeTooltip(plot);

  const all = series.flatMap((s) => s.values);
  const ticks = niceTicks(Math.min(...all), Math.max(...all));
  const yMin = ticks[0];
  const yMax = ticks[ticks.length - 1];

  function draw() {
    plot.querySelector("svg")?.remove();
    const width = plot.clientWidth;
    if (!width) return;
    const m = { top: 12, right: width < 520 ? 12 : 96, bottom: 28, left: 64 };
    const w = width - m.left - m.right;
    const h = height - m.top - m.bottom;
    const sx = (i) => m.left + (i / (x.length - 1)) * w;
    const sy = (v) => m.top + (1 - (v - yMin) / (yMax - yMin)) * h;

    const svg = el("svg", { width, height, role: "img", "aria-label": ariaLabel, tabindex: 0, class: "chart-svg" });
    plot.prepend(svg);

    for (const t of ticks) {
      el("line", { x1: m.left, x2: m.left + w, y1: sy(t), y2: sy(t), class: "chart-grid" }, svg);
      el("text", { x: m.left - 10, y: sy(t) + 4, "text-anchor": "end", class: "chart-axis" }, svg).textContent =
        yFormat(t);
    }
    const labelEvery = Math.ceil(x.length / (width < 520 ? 3 : 6));
    for (let i = 0; i < x.length; i += labelEvery) {
      el("text", { x: sx(i), y: height - 8, "text-anchor": "middle", class: "chart-axis" }, svg).textContent = xFormat(
        x[i],
      );
    }

    for (const s of series) {
      const d = s.values.map((v, i) => `${i ? "L" : "M"}${sx(i).toFixed(1)},${sy(v).toFixed(1)}`).join("");
      el("path", { d, class: "chart-line", stroke: s.color }, svg);
      const lastX = sx(s.values.length - 1);
      const lastY = sy(s.values.at(-1));
      el("circle", { cx: lastX, cy: lastY, r: 4, fill: s.color, class: "chart-dot" }, svg);
      if (m.right > 40) {
        el("text", { x: lastX + 10, y: lastY + 4, class: "chart-end-label" }, svg).textContent = s.endLabel ?? s.label;
      }
    }

    const cross = el("line", { y1: m.top, y2: m.top + h, class: "chart-cross", visibility: "hidden" }, svg);
    const dots = series.map((s) => el("circle", { r: 4, fill: s.color, class: "chart-dot", visibility: "hidden" }, svg));

    let index = null;
    function focusAt(i) {
      index = Math.max(0, Math.min(x.length - 1, i));
      const cx = sx(index);
      cross.setAttribute("x1", cx);
      cross.setAttribute("x2", cx);
      cross.setAttribute("visibility", "visible");
      series.forEach((s, k) => {
        dots[k].setAttribute("cx", cx);
        dots[k].setAttribute("cy", sy(s.values[index]));
        dots[k].setAttribute("visibility", "visible");
      });
      const rows = series
        .map((s) => ({ label: s.label, value: yFormat(s.values[index], true), color: s.color, v: s.values[index] }))
        .sort((a, b) => b.v - a.v);
      tip.show(cx, sy(Math.max(...series.map((s) => s.values[index]))), xFormat(x[index], true), rows);
    }
    function clear() {
      cross.setAttribute("visibility", "hidden");
      dots.forEach((d) => d.setAttribute("visibility", "hidden"));
      tip.hide();
    }
    svg.addEventListener("pointermove", (e) => {
      const r = svg.getBoundingClientRect();
      const i = Math.round(((e.clientX - r.left - m.left) / w) * (x.length - 1));
      focusAt(i);
    });
    svg.addEventListener("pointerleave", clear);
    svg.addEventListener("blur", clear);
    svg.addEventListener("focus", () => focusAt(index ?? x.length - 1));
    svg.addEventListener("keydown", (e) => {
      const step = e.shiftKey ? 10 : 1;
      if (e.key === "ArrowRight") focusAt((index ?? 0) + step);
      else if (e.key === "ArrowLeft") focusAt((index ?? 0) - step);
      else return;
      e.preventDefault();
    });
  }

  new ResizeObserver(draw).observe(plot);
  draw();
}

/**
 * Scatter plot with an optional diagonal reference line (y = x) and a
 * horizontal reference (y = value).
 */
export function scatterChart(host, { points, groups, xLabel, yLabel, format, diagonal, hline, height = 420, ariaLabel }) {
  host.classList.add("chart");
  host.replaceChildren();
  legend(host, groups, "dot");
  const plot = document.createElement("div");
  plot.className = "chart-plot";
  host.appendChild(plot);
  const tip = makeTooltip(plot);

  const xs = points.map((p) => p.x);
  const ys = points.map((p) => p.y);
  const xTicks = niceTicks(Math.min(...xs), Math.max(...xs), 6);
  const yTicks = niceTicks(Math.min(...ys, hline?.value ?? Infinity), Math.max(...ys, hline?.value ?? -Infinity), 5);
  const [xMin, xMax] = [xTicks[0], xTicks.at(-1)];
  const [yMin, yMax] = [yTicks[0], yTicks.at(-1)];
  const colorOf = Object.fromEntries(groups.map((g) => [g.key, g.color]));

  function draw() {
    plot.querySelector("svg")?.remove();
    const width = plot.clientWidth;
    if (!width) return;
    const wide = width >= 560;
    const m = { top: 16, right: wide ? 104 : 20, bottom: 48, left: 60 };
    const w = width - m.left - m.right;
    const h = height - m.top - m.bottom;
    const sx = (v) => m.left + ((v - xMin) / (xMax - xMin)) * w;
    const sy = (v) => m.top + (1 - (v - yMin) / (yMax - yMin)) * h;

    const svg = el("svg", { width, height, role: "img", "aria-label": ariaLabel, class: "chart-svg" });
    plot.prepend(svg);
    const clipId = `clip-${Math.random().toString(36).slice(2)}`;
    const clip = el("clipPath", { id: clipId }, el("defs", {}, svg));
    el("rect", { x: m.left, y: m.top, width: w, height: h }, clip);

    for (const t of yTicks) {
      el("line", { x1: m.left, x2: m.left + w, y1: sy(t), y2: sy(t), class: "chart-grid" }, svg);
      el("text", { x: m.left - 10, y: sy(t) + 4, "text-anchor": "end", class: "chart-axis" }, svg).textContent = format(t);
    }
    for (const t of xTicks) {
      el("line", { x1: sx(t), x2: sx(t), y1: m.top, y2: m.top + h, class: "chart-grid" }, svg);
      el("text", { x: sx(t), y: m.top + h + 18, "text-anchor": "middle", class: "chart-axis" }, svg).textContent = format(t);
    }
    el("text", { x: m.left + w / 2, y: height - 6, "text-anchor": "middle", class: "chart-axis-title" }, svg).textContent = xLabel;
    const yt = el("text", { x: 14, y: m.top + h / 2, "text-anchor": "middle", class: "chart-axis-title", transform: `rotate(-90 14 ${m.top + h / 2})` }, svg);
    yt.textContent = yLabel;

    if (diagonal) {
      const lo = Math.max(xMin, yMin);
      const hi = Math.min(xMax, yMax);
      el("line", { x1: sx(lo), y1: sy(lo), x2: sx(hi), y2: sy(hi), class: "chart-ref", "clip-path": `url(#${clipId})` }, svg);
      el("text", { x: sx(hi) + 6, y: sy(hi) + 4, class: "chart-ref-label" }, svg).textContent = diagonal;
    }
    if (hline) {
      el("line", { x1: m.left, x2: m.left + w, y1: sy(hline.value), y2: sy(hline.value), class: "chart-ref" }, svg);
      const label = wide
        ? { x: m.left + w + 8, y: sy(hline.value) + 4 }
        : { x: m.left + w - 6, y: sy(hline.value) - 6, "text-anchor": "end" };
      el("text", { ...label, class: "chart-ref-label" }, svg).textContent = hline.label;
    }

    for (const p of points) {
      const g = el("g", { class: "chart-point", tabindex: 0, role: "img", "aria-label": `${p.label}: ${xLabel} ${format(p.x)}, ${yLabel} ${format(p.y)}` }, svg);
      el("circle", { cx: sx(p.x), cy: sy(p.y), r: 12, class: "chart-hit" }, g);
      el("circle", { cx: sx(p.x), cy: sy(p.y), r: 5, fill: colorOf[p.group], class: "chart-dot" }, g);
      const show = () =>
        tip.show(sx(p.x), sy(p.y), p.label, [
          { label: xLabel, value: format(p.x), color: colorOf[p.group] },
          { label: yLabel, value: format(p.y), color: colorOf[p.group] },
        ]);
      g.addEventListener("pointerenter", show);
      g.addEventListener("focus", show);
      g.addEventListener("pointerleave", () => tip.hide());
      g.addEventListener("blur", () => tip.hide());
    }
  }

  new ResizeObserver(draw).observe(plot);
  draw();
}

/**
 * Horizontal bars growing left or right from zero, one row per item, in the
 * order given. Each row: { label, value, color, detail: [{label, value}] }.
 * An optional reference line marks a benchmark value.
 */
export function barChart(host, { rows, format, tickFormat = format, reference, ariaLabel }) {
  host.classList.add("chart");
  host.replaceChildren();
  const plot = document.createElement("div");
  plot.className = "chart-plot";
  host.appendChild(plot);
  const tip = makeTooltip(plot);

  const measure = document.createElement("canvas").getContext("2d");
  const longestLabel = () => {
    measure.font = `14px ${getComputedStyle(host).fontFamily}`;
    return Math.max(...rows.map((r) => measure.measureText(r.label).width));
  };

  const values = rows.map((r) => r.value);
  const lo = Math.min(0, ...values);
  const hi = Math.max(0, ...values, reference?.value ?? 0);

  function draw() {
    plot.querySelector("svg")?.remove();
    const width = plot.clientWidth;
    if (!width) return;
    const narrow = width < 620;
    const ticks = niceTicks(lo, hi, narrow ? 3 : 5);
    const [min, max] = [ticks[0], ticks.at(-1)];
    const bar = 22;
    const labelW = narrow ? 0 : Math.min(Math.ceil(longestLabel()) + 16, width * 0.42);
    const rowH = narrow ? 58 : 44;
    // Room on both sides for value labels at the bar tips.
    const m = { top: reference ? 26 : 8, right: 64, bottom: 28, left: labelW + (lo < 0 ? 60 : 8) };
    const w = width - m.left - m.right;
    const height = m.top + rows.length * rowH + m.bottom;
    const sx = (v) => m.left + ((v - min) / (max - min)) * w;

    const svg = el("svg", { width, height, role: "img", "aria-label": ariaLabel, class: "chart-svg" });
    plot.prepend(svg);

    for (const t of ticks) {
      el("line", { x1: sx(t), x2: sx(t), y1: m.top, y2: height - m.bottom, class: t === 0 ? "chart-baseline" : "chart-grid" }, svg);
      el("text", { x: sx(t), y: height - 8, "text-anchor": "middle", class: "chart-axis" }, svg).textContent = tickFormat(t);
    }

    rows.forEach((row, i) => {
      const top = m.top + i * rowH;
      const barY = narrow ? top + 26 : top + (rowH - bar) / 2;
      const labelY = narrow ? top + 16 : barY + bar / 2 + 4;
      const labelX = narrow ? 0 : m.left - 16 - (lo < 0 ? 52 : 0);
      el("text", { x: labelX, y: labelY, "text-anchor": narrow ? "start" : "end", class: "chart-row-label" }, svg).textContent = row.label;

      const x0 = sx(0);
      const x1 = sx(row.value);
      const left = Math.min(x0, x1);
      const len = Math.abs(x1 - x0);
      const r = Math.min(4, len);
      // Square at the baseline, rounded at the data end.
      const d =
        row.value >= 0
          ? `M${left},${barY}h${len - r}a${r},${r} 0 0 1 ${r},${r}v${bar - 2 * r}a${r},${r} 0 0 1 ${-r},${r}h${-(len - r)}z`
          : `M${x0},${barY}h${-(len - r)}a${r},${r} 0 0 0 ${-r},${r}v${bar - 2 * r}a${r},${r} 0 0 0 ${r},${r}h${len - r}z`;
      const g = el("g", { class: "chart-bar", tabindex: 0, role: "img", "aria-label": `${row.label}: ${format(row.value)}` }, svg);
      el("rect", { x: m.left, y: top, width: w, height: rowH, class: "chart-hit" }, g);
      el("path", { d, fill: row.color }, g);
      const valueX = row.value >= 0 ? x1 + 8 : x1 - 8;
      el("text", { x: valueX, y: barY + bar / 2 + 4, "text-anchor": row.value >= 0 ? "start" : "end", class: "chart-value" }, g).textContent = format(row.value);

      const show = () =>
        tip.show(Math.max(x0, x1), barY, row.label, [
          { label: "", value: format(row.value), color: row.color },
          ...(row.detail ?? []).map((d) => ({ label: d.label, value: d.value, color: "transparent" })),
        ]);
      g.addEventListener("pointerenter", show);
      g.addEventListener("focus", show);
      g.addEventListener("pointerleave", () => tip.hide());
      g.addEventListener("blur", () => tip.hide());
    });

    if (reference) {
      const x = sx(reference.value);
      el("line", { x1: x, x2: x, y1: m.top - 6, y2: height - m.bottom, class: "chart-ref" }, svg);
      el("text", { x, y: m.top - 12, "text-anchor": "middle", class: "chart-ref-label" }, svg).textContent = reference.label;
    }
  }

  new ResizeObserver(draw).observe(plot);
  draw();
}

/** A <details> table of the chart's data, so nothing is hover-only. */
export function tableView(host, { summary, columns, rows }) {
  const details = document.createElement("details");
  details.className = "chart-table";
  const s = document.createElement("summary");
  s.textContent = summary;
  details.appendChild(s);
  const wrap = document.createElement("div");
  wrap.className = "chart-table-scroll";
  const table = document.createElement("table");
  const thead = table.createTHead().insertRow();
  for (const c of columns) {
    const th = document.createElement("th");
    th.textContent = c;
    thead.appendChild(th);
  }
  const tbody = table.createTBody();
  for (const r of rows) {
    const tr = tbody.insertRow();
    for (const cell of r) tr.insertCell().textContent = cell;
  }
  wrap.appendChild(table);
  details.appendChild(wrap);
  host.appendChild(details);
}
