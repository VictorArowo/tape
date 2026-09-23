// Number formatting shared by the results and the demo.

const money0 = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD", maximumFractionDigits: 0 });
const money2 = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD", minimumFractionDigits: 2, maximumFractionDigits: 2 });
const count = new Intl.NumberFormat("en-US");
const months = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

export const fmt = {
  money: (v) => money0.format(v),
  moneyExact: (v) => money2.format(v),
  count: (v) => count.format(v),
  percent: (v, signed = true) => {
    const s = `${Math.abs(v * 100).toFixed(1)}%`;
    if (!signed) return s;
    return v > 0 ? `+${s}` : v < 0 ? `−${s}` : s;
  },
  ratio: (v) => (v == null ? "—" : v < 0 ? `−${Math.abs(v).toFixed(2)}` : v.toFixed(2)),
  date: (iso) => {
    const [y, m, d] = iso.split("-").map(Number);
    return `${d} ${months[m - 1]} ${y}`;
  },
  /** A compact period: "Jun–Aug 2026", "Mar 2025–Jan 2026" or "2023–2024". */
  period: (from, to) => {
    const [fy, fm] = from.split("-").map(Number);
    const [ty, tm] = to.split("-").map(Number);
    const mon = (m) => months[m - 1].slice(0, 3);
    if (fm === 1 && tm === 12) return fy === ty ? `${fy}` : `${fy}–${ty}`;
    if (fy === ty) return fm === tm ? `${mon(fm)} ${fy}` : `${mon(fm)}–${mon(tm)} ${fy}`;
    return `${mon(fm)} ${fy}–${mon(tm)} ${ty}`;
  },
  monthShort: (iso) => {
    const [y, m] = iso.split("-").map(Number);
    return `${months[m - 1].slice(0, 3)} ${String(y).slice(2)}`;
  },
};

/** Validated series colors (see the dataviz palette check). */
export const colors = {
  tape: "#b08a1e",
  naive: "#6d7be4",
  /** De-emphasized marks: everything that isn't the story. */
  neutral: "#737487",
  /** Hollow rings for rejected strategies; validated against tape and naive. */
  rejected: "#a0a0ac",
};
