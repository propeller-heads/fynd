// Token Flow graph for the APEX batch explorer pages.
//
// A port of propellerswap-frontend's src/components/Explore/TokenFlow/visNetwork.ts
// (the "tokens" view) and TokenFlowGraph.tsx onto plain vis-network 10, so a batch
// renders with the same engine, layout, styling and tooltips as the Turbine
// settlement explorer. Keep the two in step when either changes.
//
// Edge direction follows the frontend: an edge points from the token its hub SELLS
// to the token it BUYS. A user sells src and buys dst; a pool sells src (pays it
// out) and buys dst (takes it in) — so a pool arrow runs opposite the user flow it
// services, and reciprocal arrows land on the same pair of nodes.

const TOOLTIP_FONT = '"Geist Variable", -apple-system, "Segoe UI", Roboto, sans-serif';

// theme/foundations/colors.ts, via EXPLORE_PALETTE.
const P = {
    carbon: "#1D2021",
    cloud: "#F5F5F5",
    cloud600: "rgba(245, 245, 245, 0.64)",
    cloud400: "rgba(245, 245, 245, 0.40)",
    cloud200: "rgba(245, 245, 245, 0.20)",
    cloud150: "rgba(245, 245, 245, 0.10)",
    folly: "#FF3366",
    aquamarine: "#00FFBB",
};

// explore.constants.ts PROTO_COLORS / PROTO_LABELS, plus the protocol names Tycho
// reports that the frontend never sees (vm: prefixes, versioned forks).
const PROTO_COLORS = {
    user: "#ff3366",
    uniswap_v2: "#ff85b5",
    sushiswap_v2: "#ff85b5",
    pancakeswap_v2: "#ff85b5",
    uniswap_v3: "#b692ff",
    uniswap_v4: "#00cfff",
    ekubo_v2: "#d65dff",
    ekubo_v3: "#d65dff",
    pancakeswap_v3: "#f0c75a",
    curve: "#00ffbb",
    "vm:curve": "#00ffbb",
    balancer_v2: "#ffcc00",
    "vm:balancer_v2": "#ffcc00",
    turbine_pool: "#00ffbb",
    smart_order: "#ff8855",
    weth_wrap: "#627eea",
    multihop: "#a8e05f",
    fluid: "#1e6bff",
    fluid_v1: "#1e6bff",
    unknown: "#6e7681",
};

const PROTO_LABELS = {
    uniswap_v2: "Uniswap v2",
    sushiswap_v2: "SushiSwap v2",
    pancakeswap_v2: "PancakeSwap v2",
    uniswap_v3: "Uniswap v3",
    uniswap_v4: "Uniswap v4",
    ekubo_v2: "Ekubo v2",
    ekubo_v3: "Ekubo v3",
    pancakeswap_v3: "PancakeSwap v3",
    curve: "Curve",
    "vm:curve": "Curve",
    balancer_v2: "Balancer v2",
    "vm:balancer_v2": "Balancer v2",
    turbine_pool: "Turbine pool",
    smart_order: "Smart order (RFQ)",
    weth_wrap: "WETH wrap",
    multihop: "Multi-hop route",
    fluid: "Fluid",
    fluid_v1: "Fluid",
    unknown: "Unknown pool",
};

const DISPLAY_DECIMALS = 6;

/** format.ts fmtAmount: raw integer string → rounded, thousands-separated amount. */
function fmtAmount(rawStr, decimals) {
    if (decimals == null) return rawStr;
    const raw = BigInt(rawStr);
    const negative = raw < 0n;
    const abs = negative ? -raw : raw;
    if (decimals === 0) return (negative ? "-" : "") + abs.toString();
    const places = Math.min(decimals, DISPLAY_DECIMALS);
    const divisor = 10n ** BigInt(decimals - places);
    const rounded = (abs + divisor / 2n) / divisor;
    const base = 10n ** BigInt(places);
    const whole = rounded / base;
    const frac = rounded % base;
    const fracStr = frac.toString().padStart(places, "0").replace(/0+$/, "");
    const wholeFmt = whole.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
    const body = fracStr ? `${wholeFmt}.${fracStr}` : wholeFmt;
    return negative ? `-${body}` : body;
}

function shortAddr(a) {
    if (!a) return "";
    return a.length <= 10 ? a : `${a.slice(0, 6)}…${a.slice(-4)}`;
}

function edgeColor(e) {
    if (e.kind === "user") return PROTO_COLORS.user;
    return PROTO_COLORS[e.protocol] ?? PROTO_COLORS.unknown;
}

function edgeKindLabel(e) {
    if (e.kind === "user") return "User trade";
    return PROTO_LABELS[e.protocol] ?? "Pool swap";
}

function parseHex(hex) {
    const h = (hex || "").replace("#", "").trim();
    if (h.length !== 6) return null;
    return [
        parseInt(h.slice(0, 2), 16),
        parseInt(h.slice(2, 4), 16),
        parseInt(h.slice(4, 6), 16),
    ];
}

function hexToRgba(hex, alpha) {
    const rgb = parseHex(hex) ?? [245, 245, 245];
    return `rgba(${rgb.join(", ")}, ${alpha})`;
}

/**
 * The control point vis-network puts on a curvedCW/curvedCCW edge, mirroring
 * BezierEdgeStatic._getViaCoordinates. Note the y-up frame it works in. We need the
 * same point to place the amount labels on a curve, and vis exposes neither — so
 * this stays in step with the pinned vis-network version by hand.
 */
function viaPoint(from, to, curve) {
    const dx = to.x - from.x;
    const dy = from.y - to.y;
    const radius = Math.sqrt(dx * dx + dy * dy);
    const k = curve.roundness * 0.5 + 0.5;
    const theta = Math.atan2(dy, dx);
    const angle =
        curve.type === "curvedCW"
            ? theta + k * Math.PI
            : theta + (0.5 - curve.roundness * 0.5) * Math.PI;
    return {
        x: from.x + k * radius * Math.sin(angle),
        y: from.y + k * radius * Math.cos(angle),
    };
}

/** Point at `t` along an edge's quadratic bezier (0 at `from`, 1 at `to`). */
function curvePoint(from, to, curve, t) {
    const via = viaPoint(from, to, curve);
    const u = 1 - t;
    return {
        x: u * u * from.x + 2 * u * t * via.x + t * t * to.x,
        y: u * u * from.y + 2 * u * t * via.y + t * t * to.y,
    };
}

/** The visual bits every flow arrow shares. */
function arrowStyle(color, isUser) {
    return {
        arrows: { to: { enabled: true, scaleFactor: isUser ? 0.8 : 0.6, type: "arrow" } },
        width: isUser ? 2.8 : 1.6,
        color: { color, highlight: color, hover: color, opacity: isUser ? 1 : 0.85 },
    };
}

// Parallel flows between the same two tokens would sit on top of each other, so
// each flow bends a little differently by index.
const flowRoundness = (i) => 0.15 + 0.08 * (i % 5);

function tokenNodes(graph, tokenById) {
    // Sized by how many flows touch the token, so the busiest tokens read as the
    // hubs of the batch.
    const degree = new Map();
    for (const e of graph.edges) {
        degree.set(e.src, (degree.get(e.src) ?? 0) + 1);
        degree.set(e.dst, (degree.get(e.dst) ?? 0) + 1);
    }
    return graph.nodes.map((n) => ({
        id: n.id,
        label: n.symbol || shortAddr(n.address),
        shape: "dot",
        size: 12 + 4 * Math.sqrt(degree.get(n.id) ?? 1),
        color: {
            background: P.cloud,
            border: P.cloud200,
            highlight: { background: P.cloud, border: P.cloud },
            hover: { background: P.cloud, border: P.cloud },
        },
        font: {
            color: P.cloud,
            size: 13,
            face: TOOLTIP_FONT,
            strokeWidth: 4,
            strokeColor: P.carbon,
            vadjust: -2,
        },
        borderWidth: 1.5,
        title: nodeTooltip(n, graph, tokenById),
    }));
}

/** One arrow per flow, from the token its hub sells to the token it buys. */
function buildNetworkData(graph) {
    const tokenById = new Map(graph.nodes.map((n) => [n.id, n]));
    const edges = [];
    const amountLabels = [];
    graph.edges.forEach((e, i) => {
        const isUser = e.kind === "user";
        const color = edgeColor(e);
        const curve = { type: "curvedCW", roundness: flowRoundness(i) };
        edges.push({
            id: `e${i}`,
            from: e.src,
            to: e.dst,
            label: edgeKindLabel(e),
            font: {
                color,
                size: 11,
                face: TOOLTIP_FONT,
                strokeWidth: 4,
                strokeColor: P.carbon,
                align: "top",
            },
            ...arrowStyle(color, isUser),
            smooth: { enabled: true, ...curve },
            title: edgeTooltip(e, tokenById),
        });
        amountLabels.push({
            from: e.src,
            to: e.dst,
            curve,
            text: `${fmtAmount(e.src_amount, tokenById.get(e.src)?.decimals)} → ${fmtAmount(e.dst_amount, tokenById.get(e.dst)?.decimals)}`,
            color,
            fontSize: isUser ? 12 : 11,
        });
    });
    return { nodes: tokenNodes(graph, tokenById), edges, amountLabels, tokenById };
}

function tokenLabel(id, tokenById) {
    return tokenById.get(id)?.symbol || shortAddr(id);
}

const NETWORK_OPTIONS = {
    physics: {
        solver: "forceAtlas2Based",
        forceAtlas2Based: {
            gravitationalConstant: -90,
            centralGravity: 0.005,
            springLength: 180,
            springConstant: 0.08,
            avoidOverlap: 0.7,
            damping: 0.5,
        },
        stabilization: { iterations: 400, fit: true },
        minVelocity: 0.5,
    },
    interaction: {
        hover: true,
        tooltipDelay: 100,
        dragNodes: true,
        multiselect: false,
        zoomView: true,
    },
    nodes: { borderWidth: 1.5 },
    edges: { selectionWidth: 3 },
};

// Don't magnify a small graph past this; three nodes filling the canvas looks
// broken. Plus a little slack so nothing sits flush against the edge.
const MAX_FIT_SCALE = 1;
const FIT_MARGIN = 0.94;
const INSETS = { top: 12, right: 12, bottom: 12, left: 12 };

/** Like network.fit(), but keeping a margin clear on every side. */
function fitToVisibleArea(network, data, viewport, insets, animation) {
    const bounds = data.nodes.reduce((acc, n) => {
        const box = network.getBoundingBox(String(n.id));
        if (!box || !Number.isFinite(box.left)) return acc;
        if (!acc) return { ...box };
        return {
            left: Math.min(acc.left, box.left),
            right: Math.max(acc.right, box.right),
            top: Math.min(acc.top, box.top),
            bottom: Math.max(acc.bottom, box.bottom),
        };
    }, null);
    if (!bounds) {
        network.fit({ animation });
        return;
    }
    const visibleW = Math.max(120, viewport.width - insets.left - insets.right);
    const visibleH = Math.max(120, viewport.height - insets.top - insets.bottom);
    const scale = Math.min(
        (visibleW / Math.max(bounds.right - bounds.left, 1)) * FIT_MARGIN,
        (visibleH / Math.max(bounds.bottom - bounds.top, 1)) * FIT_MARGIN,
        MAX_FIT_SCALE
    );
    const offsetX = (insets.left - insets.right) / 2;
    const offsetY = (insets.top - insets.bottom) / 2;
    network.moveTo({
        position: {
            x: (bounds.left + bounds.right) / 2 - offsetX / scale,
            y: (bounds.top + bounds.bottom) / 2 - offsetY / scale,
        },
        scale,
        animation,
    });
}

/** Amount labels at each edge's curve midpoint, drawn on top of all edges. */
function drawAmountLabels(ctx, network, labels) {
    const positions = network.getPositions();
    ctx.save();
    ctx.textBaseline = "middle";
    ctx.textAlign = "center";
    for (const label of labels) {
        const from = positions[label.from];
        const to = positions[label.to];
        if (!from || !to || !label.text) continue;
        ctx.font = `${label.fontSize}px ${TOOLTIP_FONT}`;
        const { x: lx, y: ly } = curvePoint(from, to, label.curve, 0.5);
        const padX = 6;
        const padY = 4;
        const w = ctx.measureText(label.text).width + padX * 2;
        const h = label.fontSize + padY * 2;
        ctx.fillStyle = hexToRgba(label.color, 0.22);
        roundRect(ctx, lx, ly, w, h);
        ctx.fill();
        ctx.fillStyle = P.cloud;
        ctx.fillText(label.text, lx, ly);
    }
    ctx.restore();
}

function roundRect(ctx, cx, cy, w, h) {
    const rad = h / 2;
    ctx.beginPath();
    ctx.moveTo(cx - w / 2 + rad, cy - h / 2);
    ctx.lineTo(cx + w / 2 - rad, cy - h / 2);
    ctx.arc(cx + w / 2 - rad, cy, rad, -Math.PI / 2, Math.PI / 2);
    ctx.lineTo(cx - w / 2 + rad, cy + h / 2);
    ctx.arc(cx - w / 2 + rad, cy, rad, Math.PI / 2, -Math.PI / 2);
    ctx.closePath();
}

// ---------- tooltips (HTMLElement, styled inline to match the theme) ----------

// Token symbols come straight from arbitrary on-chain symbol() strings, and these
// bodies are built with innerHTML — escape them.
function esc(s) {
    return String(s)
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;");
}

function ttShell(borderColor, headBg, headText, body) {
    const wrap = document.createElement("div");
    wrap.style.cssText = `background:${P.carbon};color:${P.cloud};border:1.5px solid ${borderColor};border-radius:6px;overflow:hidden;box-shadow:0 4px 48px rgba(0,0,0,0.5);min-width:220px;font-family:${TOOLTIP_FONT}`;
    wrap.innerHTML = `<div style="padding:5px 12px;font-size:10px;text-transform:uppercase;letter-spacing:0.05em;font-weight:600;color:${P.carbon};background:${headBg}">${esc(headText)}</div><div style="padding:10px 12px">${body}</div>`;
    return wrap;
}

function nodeTooltip(n, graph, tokenById) {
    const outs = graph.edges.filter((e) => e.src === n.id);
    const ins = graph.edges.filter((e) => e.dst === n.id);
    // Every flow amount is denominated in this node's token, so suffix the symbol —
    // the row's other token is the counterparty, not the unit.
    const sym = esc(n.symbol || shortAddr(n.address));
    const flowRow = (arrow, token, amt, color) => `
        <div style="display:flex;justify-content:space-between;gap:16px;font-size:12px;margin-top:4px">
            <span style="color:${P.cloud600}">${arrow} ${token}</span>
            <span style="color:${color}">${amt}</span>
        </div>`;
    const flows =
        outs.length || ins.length
            ? `<div style="margin-top:8px;padding-top:6px;border-top:1px solid ${P.cloud150}">
                ${outs
                    .map((e) =>
                        flowRow(
                            "→",
                            esc(tokenLabel(e.dst, tokenById)),
                            `−${fmtAmount(e.src_amount, n.decimals)} ${sym}`,
                            P.folly
                        )
                    )
                    .join("")}
                ${ins
                    .map((e) =>
                        flowRow(
                            "←",
                            esc(tokenLabel(e.src, tokenById)),
                            `+${fmtAmount(e.dst_amount, n.decimals)} ${sym}`,
                            P.aquamarine
                        )
                    )
                    .join("")}
               </div>`
            : "";
    const body = `
        <div style="font-weight:600;font-size:13px">${esc(n.symbol || shortAddr(n.address))}</div>
        <div style="color:${P.cloud400};font-size:11px;margin-top:4px">${esc(n.address)}</div>
        <div style="color:${P.cloud600};font-size:11px;margin-top:4px">decimals: ${n.decimals ?? "?"}</div>
        ${flows}`;
    return ttShell(P.cloud, P.cloud, "Token", body);
}

function edgeTooltip(e, tokenById) {
    const color = edgeColor(e);
    const hubLabel = e.kind === "user" ? `tx ${e.hub}` : `at ${e.hub}`;
    const srcAmt = fmtAmount(e.src_amount, tokenById.get(e.src)?.decimals);
    const dstAmt = fmtAmount(e.dst_amount, tokenById.get(e.dst)?.decimals);
    const note = e.note
        ? `<div style="margin-top:6px;color:${P.cloud600};font-size:11px">${esc(e.note)}</div>`
        : "";
    const body = `
        <div style="font-size:12px">
            <span style="color:${P.cloud}">${srcAmt}</span> ${esc(tokenLabel(e.src, tokenById))}
            <span style="color:${P.cloud400}"> → </span>
            <span style="color:${P.cloud}">${dstAmt}</span> ${esc(tokenLabel(e.dst, tokenById))}
        </div>
        ${note}
        <div style="margin-top:6px;color:${P.cloud400};font-size:11px">${esc(hubLabel)}</div>`;
    return ttShell(color, color, edgeKindLabel(e), body);
}

// ---------- canvas wiring (TokenFlowGraph.tsx) ----------

// The kind label and the amount overlay would sit on top of each other, so only one
// shows at a time. Shrink the font instead of blanking `label`: vis-network's option
// parser silently ignores an empty-string label update, but `font` is a normal
// deep-mergeable field, so a near-zero size reliably hides the text.
function withLabelVisibility(edges, visible) {
    return edges.map((e) =>
        typeof e.font === "object" && e.font
            ? { ...e, font: { ...e.font, size: visible ? e.font.size : 0.01 } }
            : e
    );
}

function labelVisibilityUpdates(edges, visible) {
    return edges.flatMap((e) =>
        typeof e.font === "object" && e.font
            ? [{ id: e.id, font: { ...e.font, size: visible ? e.font.size : 0.01 } }]
            : []
    );
}

/**
 * Draw `graph` into `container` and return a handle. The container must be visible
 * and sized when this runs — vis-network measures it to lay the graph out.
 */
function renderTokenFlow(container, graph) {
    const data = buildNetworkData(graph);
    let showAmounts = false;
    const edgesData = new vis.DataSet(withLabelVisibility(data.edges, !showAmounts));
    const network = new vis.Network(
        container,
        { nodes: new vis.DataSet(data.nodes), edges: edgesData },
        NETWORK_OPTIONS
    );
    network.on("afterDrawing", (ctx) => {
        if (showAmounts) drawAmountLabels(ctx, network, data.amountLabels);
    });
    network.once("stabilizationIterationsDone", () => {
        // Physics off from here on, so a drag is the only thing that moves a node.
        network.setOptions({ physics: { enabled: false } });
        fitToVisibleArea(
            network,
            data,
            { width: container.clientWidth, height: container.clientHeight },
            INSETS,
            { duration: 300, easingFunction: "easeInOutQuad" }
        );
    });
    return {
        network,
        setAmounts(next) {
            showAmounts = next;
            edgesData.update(labelVisibilityUpdates(data.edges, !next));
            network.redraw();
        },
    };
}
