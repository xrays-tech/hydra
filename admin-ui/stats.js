/* ===========================================================================
 * Usage Stats (指标统计) — compare token usage & request counts across
 * tenants and providers with lightweight vanilla-JS charts.
 *
 * Plain ES2017+, no build step, no external deps. Rendered by renderStats()
 * (registered in app.js as the "stats" custom page — loaded BEFORE app.js so
 * the global is visible when app.js builds its CUSTOM table; same pattern as
 * api-docs.js). Data comes from GET /api/v1/stats/usage, which aggregates the
 * live prometheus counters (hydra_requests_total / hydra_tokens_total) —
 * cumulative since process start, not time-windowed.
 * ======================================================================== */

"use strict";

let _statsTimer = null;

function renderStats() {
  if (_statsTimer) { clearInterval(_statsTimer); _statsTimer = null; }

  const content = $("#content");
  clear(content);

  // page actions: auto-refresh toggle + manual refresh
  clear($("#page-actions"));
  const auto = el("label", { class: "toggle-pill", title: "refresh every 10s" },
    el("input", { type: "checkbox", id: "stats-autorefresh" }),
    el("span", { text: "auto" }),
  );
  $("#page-actions").appendChild(auto);
  $("#stats-autorefresh").addEventListener("change", (e) => {
    if (e.target.checked) _statsTimer = setInterval(renderStats, 10000);
    else { clearInterval(_statsTimer); _statsTimer = null; }
  });
  $("#page-actions").appendChild(
    el("button", { class: "btn sm", onClick: renderStats },
      icon("refresh", 14), el("span", { class: "btn-label", text: "Refresh" })),
  );

  // skeleton
  content.appendChild(el("div", { class: "panel" },
    el("div", { class: "stat-grid", style: "grid-template-columns:repeat(auto-fit,minmax(150px,1fr))" },
      Array.from({ length: 6 }).map(() =>
        el("div", { class: "stat" },
          el("span", { class: "skeleton", style: "width:100%;height:100%;display:block" })),
      ),
    ),
  ));

  api("GET", "/stats/usage").then(renderStatsData).catch((e) => {
    clear(content);
    content.appendChild(emptyState("alert", "Couldn't load stats", e.message));
    toast(e.message, "err", { title: "Failed to load usage stats" });
  });
}

function renderStatsData(d) {
  const content = $("#content");
  clear(content);

  const t = d.totals || {};
  const cards = [
    { l: "requests", v: fmtNum(t.requests) },
    { l: "tokens", v: fmtNum(t.tokens) },
    { l: "prompt tokens", v: fmtNum(t.tokens_prompt) },
    { l: "completion tokens", v: fmtNum(t.tokens_completion) },
    { l: "tenants", v: String(t.tenants || 0) },
    { l: "providers", v: String(t.providers || 0) },
  ];

  content.appendChild(el("div", { class: "panel" },
    el("div", { class: "panel-head" },
      el("h2", {}, el("span", { text: "Totals" })),
      el("div", { class: "spacer" }),
      el("span", { class: "muted", text: "updated " + (shortTime(d.generated_at) || "-") }),
    ),
    el("div", { class: "stat-grid", id: "stats-totals" }),
  ));
  const grid = $("#stats-totals");
  for (const c of cards) {
    grid.appendChild(el("div", { class: "stat" },
      el("div", { class: "sl", text: c.l }),
      el("div", { class: "sv", text: c.v }),
    ));
  }

  const noData = (t.requests || 0) === 0 && (t.tokens || 0) === 0;
  if (noData) {
    content.appendChild(el("div", { class: "panel" },
      emptyState("chart", "No usage recorded yet",
        "The counters are cumulative since process start. Send a few proxied requests (or check /metrics) and hit Refresh."),
    ));
    return;
  }

  content.appendChild(renderDimensionPanel("By tenant", d.by_tenant || []));
  content.appendChild(renderDimensionPanel("By provider", d.by_provider || []));

  content.appendChild(el("p", { class: "note" },
    "Counters are cumulative since the process started (prometheus counters). " +
    "For trend-over-time analytics, scrape /metrics into an external BI system."));
}

function renderDimensionPanel(title, rows) {
  return el("div", { class: "panel" },
    el("div", { class: "panel-head" },
      el("h2", {}, el("span", { text: title }), " ",
        el("span", { class: "count", text: String(rows.length) })),
      el("div", { class: "spacer" }),
      el("div", { class: "chart-legend" },
        el("span", { class: "legend-item" }, el("i", { class: "sw sw-prompt" }), " prompt"),
        el("span", { class: "legend-item" }, el("i", { class: "sw sw-completion" }), " completion"),
      ),
    ),
    el("div", { class: "stats-grid" },
      barChart({ title: "Tokens", rows, metric: "tokens", stacked: true }),
      barChart({ title: "Requests", rows, metric: "requests" }),
    ),
  );
}

/* Horizontal bar chart (pure DOM + CSS, no chart lib). metric is the row
 * key to compare; stacked splits the bar into prompt/completion segments. */
function barChart(opts) {
  const MAX_ROWS = 20;
  const sorted = (opts.rows || []).slice().sort((a, b) =>
    ((b[opts.metric] || 0) - (a[opts.metric] || 0)) || a.name.localeCompare(b.name));
  const shown = sorted.slice(0, MAX_ROWS);
  let max = 1;
  for (const r of shown) {
    const v = r[opts.metric] || 0;
    if (v > max) max = v;
  }

  const wrap = el("div", { class: "chart" },
    el("h3", {},
      el("span", { text: opts.title }), " ",
      el("span", { class: "muted", text: shown.length < sorted.length ? "top " + shown.length + "/" + sorted.length : "" }),
    ),
    el("div", { class: "chart-rows" }),
  );
  const rowsEl = wrap.querySelector(".chart-rows");

  for (const r of shown) {
    const v = r[opts.metric] || 0;
    const pct = (v / max) * 100;
    const prompt = r.tokens_prompt || 0;
    const completion = r.tokens_completion || 0;
    const tip = r.name + ": " + v.toLocaleString() +
      (opts.stacked ? " tokens (prompt " + prompt.toLocaleString() + " / completion " + completion.toLocaleString() + ")" : " requests");
    const row = el("div", { class: "chart-row", title: tip },
      el("div", { class: "chart-label", text: r.name }),
      el("div", { class: "chart-track" }),
      el("div", { class: "chart-value", text: fmtNum(v) }),
    );
    const track = row.querySelector(".chart-track");
    if (opts.stacked) {
      const pw = (prompt / max) * 100;
      const cw = (completion / max) * 100;
      if (pw > 0) track.appendChild(el("div", { class: "bar-seg seg-prompt", style: "width:" + pw + "%" }));
      if (cw > 0) track.appendChild(el("div", { class: "bar-seg seg-completion", style: "width:" + cw + "%" }));
    } else {
      track.appendChild(el("div", { class: "bar", style: "width:" + pct + "%" }));
    }
    rowsEl.appendChild(row);
  }

  if (!shown.length) {
    rowsEl.appendChild(el("p", { class: "muted", text: "no data yet" }));
  }
  return wrap;
}

/* Compact number formatting: 1234 -> 1.23k, 1500000 -> 1.5M. */
function fmtNum(n) {
  if (typeof n !== "number" || !isFinite(n)) n = 0;
  if (n >= 1e9) return trimNum(n / 1e9) + "B";
  if (n >= 1e6) return trimNum(n / 1e6) + "M";
  if (n >= 1e4) return trimNum(n / 1e3) + "k";
  return String(n);
}
function trimNum(x) {
  return String(Math.round(x * 100) / 100);
}
