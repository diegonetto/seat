/* Pane door client. ttyd 1.7.7 speaks this socket; the page adds nothing else.
   __SEAT__ and __NEWLINE__ are replaced when the page is written. */
const SEAT = __SEAT__;
const NEWLINE = new Uint8Array(__NEWLINE__);

const status = document.getElementById("status");
const FONT = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";
const FONT_SIZE = 15;
const term = new Terminal({
  cursorBlink: true,
  fontFamily: FONT,
  fontSize: FONT_SIZE,
  scrollback: 5000,
  theme: { background: "#000000" },
});
const fit = new FitAddon.FitAddon();
term.loadAddon(fit);

// Columns are a whole number of cells, so the last column leaves a strip.
// Try sizes within half a point of 15 and keep the one that lands on the
// width. A hidden scrollbar measures as 0, and xterm then substitutes 15,
// which would bring the strip back. Anything still left is stretched.
// A maximize delivers a burst of resize events. Wait until they stop, and
// skip a size the terminal already fills.
const RESIZE_WAIT = 100;
let fitting = false;
let fittedWidth = 0;
let fittedHeight = 0;

function fitTerm() {
  const parent = term.element.parentElement;
  const width = parent.clientWidth;
  const height = parent.clientHeight;
  if (fittedWidth > 0 && width === fittedWidth && height === fittedHeight) {
    return;
  }
  term.element.style.transform = "";
  let chosen = FONT_SIZE;
  let bestWaste = width + 1;
  fitting = true;
  try {
    for (let i = 0; i <= 20; i += 1) {
      const step = Math.ceil(i / 2) * 0.05;
      const font = Math.round((FONT_SIZE + (i === 0 ? 0 : (i % 2 ? step : -step))) * 100) / 100;
      term.options.fontSize = font;
      term._core.viewport.scrollBarWidth = 0;
      fit.fit();
      const waste = width - term.element.querySelector(".xterm-screen").offsetWidth;
      if (waste < 0 || waste >= bestWaste) {
        continue;
      }
      bestWaste = waste;
      chosen = font;
      if (waste === 0) {
        break;
      }
    }
    if (term.options.fontSize !== chosen) {
      term.options.fontSize = chosen;
      term._core.viewport.scrollBarWidth = 0;
      fit.fit();
    }
  } finally {
    fitting = false;
  }
  const used = term.element.querySelector(".xterm-screen").offsetWidth;
  if (used > 0 && used < width) {
    term.element.style.transformOrigin = "0 0";
    term.element.style.transform = "scaleX(" + (width / used) + ")";
  }
  if (width > 0 && height > 0) {
    fittedWidth = width;
    fittedHeight = height;
  }
}

term.open(document.getElementById("term"));
fitTerm();
term.focus();

let socket = null;

function setStatus(text) {
  status.textContent = text;
}

function sendRaw(bytes) {
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    return;
  }
  const payload = new Uint8Array(bytes.length + 1);
  payload[0] = 0x30;
  payload.set(bytes, 1);
  socket.send(payload);
}

term.attachCustomKeyEventHandler((ev) => {
  if (ev.type !== "keydown") {
    return true;
  }
  if (ev.key !== "Enter" || !ev.shiftKey || ev.altKey || ev.ctrlKey || ev.metaKey) {
    return true;
  }
  ev.preventDefault();
  sendRaw(NEWLINE);
  return false;
});

term.onData((data) => {
  sendRaw(new TextEncoder().encode(data));
});

term.onBinary((data) => {
  const bytes = new Uint8Array(data.length);
  for (let i = 0; i < data.length; i += 1) {
    bytes[i] = data.charCodeAt(i) & 0xff;
  }
  sendRaw(bytes);
});

function sendSize(cols, rows) {
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    return;
  }
  const msg = new TextEncoder().encode("1" + JSON.stringify({ columns: cols, rows: rows }));
  socket.send(msg);
}

term.onResize(({ cols, rows }) => {
  if (fitting) {
    return;
  }
  sendSize(cols, rows);
});

let resizeTimer = 0;
window.addEventListener("resize", () => {
  window.clearTimeout(resizeTimer);
  resizeTimer = window.setTimeout(() => {
    const cols = term.cols;
    const rows = term.rows;
    fitTerm();
    if (term.cols !== cols || term.rows !== rows) {
      sendSize(term.cols, term.rows);
    }
  }, RESIZE_WAIT);
});

function bytesOf(data) {
  if (typeof data === "string") {
    return new TextEncoder().encode(data);
  }
  return new Uint8Array(data);
}

async function connect() {
  const base = location.pathname.replace(/\/$/, "");
  const tokenUrl = base + "/token";
  let token = "";
  try {
    const resp = await fetch(tokenUrl, { cache: "no-store" });
    if (resp.ok) {
      const body = await resp.json();
      if (body && typeof body.token === "string") {
        token = body.token;
      }
    }
  } catch (err) {
    setStatus(SEAT + " — no token");
  }

  const wsUrl = (location.protocol === "https:" ? "wss://" : "ws://") + location.host + base + "/ws";
  socket = new WebSocket(wsUrl, ["tty"]);
  socket.binaryType = "arraybuffer";
  socket.addEventListener("open", () => {
    const hello = new TextEncoder().encode(JSON.stringify({
      AuthToken: token,
      columns: term.cols,
      rows: term.rows,
    }));
    socket.send(hello);
    setStatus("");
    term.focus();
  });
  socket.addEventListener("message", (ev) => {
    const raw = bytesOf(ev.data);
    if (raw.length === 0) {
      return;
    }
    const cmd = String.fromCharCode(raw[0]);
    if (cmd === "0") {
      term.write(raw.subarray(1));
    }
  });
  socket.addEventListener("close", () => {
    setStatus(SEAT + " — closed. Refresh to open it again.");
  });
  socket.addEventListener("error", () => {
    setStatus(SEAT + " — connection failed");
  });
}

window.addEventListener("error", (ev) => {
  setStatus(ev.message || "page error");
});

connect();
