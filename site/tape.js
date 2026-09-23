// The hero tape: real one-minute BTC/USDT closes printed up to a "now"
// line, with the paper blank beyond it. The future isn't drawn because a
// strategy never gets to see it.

(async function () {
  const canvas = document.getElementById("tape-canvas");
  const toggle = document.querySelector(".tape-toggle");
  const readout = {
    time: document.querySelector('[data-readout="time"]'),
    price: document.querySelector('[data-readout="price"]'),
    change: document.querySelector('[data-readout="change"]'),
  };
  if (!canvas) return;

  let data;
  try {
    data = await fetch("data/btcusdt-2024-01-03.json").then((r) => r.json());
  } catch {
    return;
  }
  const closes = data.close;
  const n = closes.length;
  const dayOpen = closes[0];
  const lo = Math.min(...closes);
  const hi = Math.max(...closes);

  // `?still` shows the same frame as reduced motion, for screenshots.
  const reducedMotion =
    window.matchMedia("(prefers-reduced-motion: reduce)").matches ||
    new URLSearchParams(location.search).has("still");
  const START = 675;          // 11:15 UTC: the window is full and the noon crash is close
  const STILL = 790;          // a frame just after the noon crash
  const MINUTES_PER_SECOND = 7;
  const WINDOW_MINUTES = 260; // how much history is visible left of "now"

  const css = getComputedStyle(document.documentElement);
  const color = (name) => css.getPropertyValue(name).trim();
  const PAPER = color("--paper");
  const INK = color("--paper-ink");
  const MARK = color("--mark");

  const ctx = canvas.getContext("2d");
  let width = 0;
  let height = 0;

  function resize() {
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const rect = canvas.getBoundingClientRect();
    width = rect.width;
    height = rect.height;
    canvas.width = Math.round(width * dpr);
    canvas.height = Math.round(height * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    draw();
  }

  const priceFmt = new Intl.NumberFormat("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  const hhmm = (m) => {
    const h = Math.floor(m / 60) % 24;
    const mm = Math.floor(m % 60);
    return `${String(h).padStart(2, "0")}:${String(mm).padStart(2, "0")}`;
  };

  let minute = reducedMotion ? STILL : START;
  let shownMinute = -1;

  function priceAt(m) {
    const i = Math.floor(m);
    if (i >= n - 1) return closes[n - 1];
    const f = m - i;
    return closes[i] + (closes[i + 1] - closes[i]) * f;
  }

  function draw() {
    if (!width) return;
    const narrow = width < 640;
    const nowX = Math.round(width * (narrow ? 0.66 : 0.74));
    const pxPerMin = nowX / (narrow ? WINDOW_MINUTES * 0.6 : WINDOW_MINUTES);
    const top = 20;
    const labelBand = 34;
    const bottom = height - labelBand;
    const pad = (hi - lo) * 0.08;
    const y = (p) => top + ((hi + pad - p) / (hi - lo + 2 * pad)) * (bottom - top);
    const xOf = (m) => nowX - (minute - m) * pxPerMin;

    ctx.fillStyle = PAPER;
    ctx.fillRect(0, 0, width, height);

    // A faint rule where the printer stamps times and prices.
    ctx.strokeStyle = INK;
    ctx.globalAlpha = 0.14;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(0, bottom + 0.5);
    ctx.lineTo(nowX, bottom + 0.5);
    ctx.stroke();
    ctx.globalAlpha = 1;

    // Printed labels at the tightest round interval that doesn't collide.
    ctx.font = `${narrow ? 12 : 13}px "Cutive Mono", monospace`;
    ctx.textBaseline = "alphabetic";
    const withPrice = !narrow;
    const sample = withPrice ? `00:00  ${priceFmt.format(hi)}` : "00:00";
    const needed = ctx.measureText(sample).width + 24;
    const every = [15, 30, 60, 120, 240].find((i) => i * pxPerMin >= needed) ?? 240;
    const first = Math.max(0, Math.ceil((minute - nowX / pxPerMin) / every) * every);
    for (let m = first; m <= minute; m += every) {
      const x = xOf(m);
      ctx.globalAlpha = 0.35;
      ctx.beginPath();
      ctx.moveTo(x + 0.5, bottom);
      ctx.lineTo(x + 0.5, bottom + 6);
      ctx.stroke();
      ctx.globalAlpha = 0.85;
      ctx.fillStyle = INK;
      const label = withPrice ? `${hhmm(m)}  ${priceFmt.format(closes[m])}` : hhmm(m);
      ctx.fillText(label, x + 5, bottom + 22);
    }
    ctx.globalAlpha = 1;

    // The price line, clipped at now.
    ctx.save();
    ctx.beginPath();
    ctx.rect(0, 0, nowX, height);
    ctx.clip();
    ctx.strokeStyle = INK;
    ctx.lineWidth = narrow ? 1.5 : 1.75;
    ctx.lineJoin = "round";
    ctx.beginPath();
    const startM = Math.max(0, Math.floor(minute - nowX / pxPerMin) - 1);
    ctx.moveTo(xOf(startM), y(closes[startM]));
    for (let m = startM + 1; m <= Math.floor(minute); m++) {
      ctx.lineTo(xOf(m), y(closes[m]));
    }
    ctx.lineTo(nowX, y(priceAt(minute)));
    ctx.stroke();
    ctx.restore();

    // Now: a highlighter stroke with a hairline of ink through it.
    ctx.fillStyle = MARK;
    ctx.globalAlpha = 0.55;
    ctx.fillRect(nowX - 5, 0, 10, height);
    ctx.globalAlpha = 1;
    ctx.fillStyle = INK;
    ctx.fillRect(nowX, 0, 1, height);
    ctx.beginPath();
    ctx.arc(nowX + 0.5, y(priceAt(minute)), 3.5, 0, Math.PI * 2);
    ctx.fill();

    updateReadout();
  }

  function updateReadout() {
    const m = Math.floor(minute);
    if (m === shownMinute) return;
    shownMinute = m;
    const price = closes[m];
    const change = ((price - dayOpen) / dayOpen) * 100;
    readout.time.textContent = `${hhmm(m)} UTC`;
    readout.price.textContent = priceFmt.format(price);
    readout.change.textContent = `${change >= 0 ? "+" : "−"}${Math.abs(change).toFixed(2)}% today`;
    readout.change.dataset.dir = change < 0 ? "down" : "up";
  }

  let playing = !reducedMotion;
  if (reducedMotion && toggle) toggle.hidden = true;
  let visible = true;
  let last = 0;
  let scheduled = false;

  function frame(t) {
    scheduled = false;
    if (!playing || !visible) {
      last = 0;
      return;
    }
    if (last) {
      const dt = Math.min((t - last) / 1000, 0.1);
      minute += dt * MINUTES_PER_SECOND;
      if (minute >= n - 1) minute = START;
    }
    last = t;
    draw();
    schedule();
  }

  // Only ever one animation loop, however many times play() is called.
  function schedule() {
    if (!scheduled) {
      scheduled = true;
      requestAnimationFrame(frame);
    }
  }

  function play() {
    if (playing && visible) schedule();
  }

  toggle?.addEventListener("click", () => {
    playing = !playing;
    toggle.textContent = playing ? "Pause" : "Play";
    toggle.setAttribute("aria-pressed", String(!playing));
    play();
  });

  new IntersectionObserver((entries) => {
    visible = entries[0].isIntersecting;
    play();
  }).observe(canvas);

  new ResizeObserver(resize).observe(canvas);
  await document.fonts.ready;
  resize();
  play();
})();
