// Generates docs/presentation/broccoli-devops-agent.pptx.
//
// One-off setup in any scratch directory (the dependencies are not part of the workspace):
//   npm init -y && npm install pptxgenjs sharp react-icons react react-dom
//   node /path/to/build-deck.js /path/to/broccoli-devops-agent.pptx
//
// Icons are lucide glyphs from react-icons, rasterized with sharp; everything else is native
// PowerPoint shapes and text, so the deck stays editable after generation.
const pptxgen = require('pptxgenjs');
const React = require('react');
const ReactDOMServer = require('react-dom/server');
const sharp = require('sharp');
const lu = require('react-icons/lu');

const OUT = process.argv[2] || 'deck.pptx';

// Palette: broccoli greens dominate, amber marks the human, red marks execution.
const C = {
  ink: '14352A', forest: '1F5A3C', moss: '7DB37F', mint: 'EEF5F0', mint2: 'D6E7DB',
  amber: 'D98E27', amberTint: 'FFF4E0', red: 'B54B3A', redTint: 'F9EBE7',
  blue: '2F5C9E', blueTint: 'E8F0FB', text: '1B2620', muted: '5E6B63', white: 'FFFFFF',
  line: 'C9D6CE', gray: 'F4F4F2', grayLine: '9AA59E', paleOnDark: 'BFD8C7',
};
const F = { head: 'Cambria', body: 'Calibri' };

const pres = new pptxgen();
pres.layout = 'LAYOUT_WIDE'; // 13.333 x 7.5
pres.author = 'Broccoli DevOps Agent';
pres.title = 'Broccoli DevOps Agent';

// ---------- helpers ----------
const iconCache = new Map();
async function iconPng(name, color) {
  const key = name + color;
  if (iconCache.has(key)) return iconCache.get(key);
  const Icon = lu[name];
  if (!Icon) throw new Error('missing icon ' + name);
  let svg = ReactDOMServer.renderToStaticMarkup(
    React.createElement(Icon, { size: 256, strokeWidth: 1.9 })
  );
  svg = svg.replace(/currentColor/g, '#' + color);
  const buf = await sharp(Buffer.from(svg)).png().toBuffer();
  const data = 'image/png;base64,' + buf.toString('base64');
  iconCache.set(key, data);
  return data;
}

async function iconCircle(slide, name, x, y, d, bg, fg) {
  slide.addShape(pres.ShapeType.ellipse, {
    x, y, w: d, h: d, fill: { color: bg }, line: { color: bg, width: 0 },
  });
  const pad = d * 0.25;
  slide.addImage({ data: await iconPng(name, fg), x: x + pad, y: y + pad, w: d - 2 * pad, h: d - 2 * pad });
}

function text(slide, str, x, y, w, h, o = {}) {
  slide.addText(str, {
    x, y, w, h,
    fontFace: o.font || F.body, fontSize: o.size || 12, color: o.color || C.text,
    bold: !!o.bold, italic: !!o.italic, align: o.align || 'left', valign: o.valign || 'top',
    margin: o.margin === undefined ? 0 : o.margin, isTextBox: true,
    fill: o.fill ? { color: o.fill } : undefined, charSpacing: o.charSpacing,
    lineSpacingMultiple: o.lineSpacing, paraSpaceAfter: o.paraAfter,
  });
}

function runs(slide, arr, x, y, w, h, o = {}) {
  // arr: [{t, bold, size, color, italic, brk}] ; brk => breakLine
  const items = arr.map((r, i) => ({
    text: r.t,
    options: {
      bold: !!r.bold, italic: !!r.italic, fontSize: r.size || o.size || 12,
      color: r.color || o.color || C.text, fontFace: r.font || o.font || F.body,
      breakLine: r.brk !== undefined ? r.brk : i < arr.length - 1,
      paraSpaceAfter: r.after, bullet: r.bullet ? (r.bullet === true ? true : r.bullet) : undefined,
      indentLevel: r.indent,
    },
  }));
  slide.addText(items, {
    x, y, w, h, margin: o.margin === undefined ? 0 : o.margin, isTextBox: true,
    align: o.align || 'left', valign: o.valign || 'top', fill: o.fill ? { color: o.fill } : undefined,
  });
}

function bullets(slide, items, x, y, w, h, o = {}) {
  const arr = items.map((t, i) => ({
    text: t,
    options: {
      bullet: { indent: 14 }, fontSize: o.size || 12, color: o.color || C.text, fontFace: F.body,
      breakLine: i < items.length - 1, paraSpaceAfter: o.after === undefined ? 6 : o.after,
    },
  }));
  slide.addText(arr, { x, y, w, h, margin: 0, isTextBox: true, valign: o.valign || 'top' });
}

function card(slide, x, y, w, h, o = {}) {
  slide.addShape(pres.ShapeType.roundRect, {
    x, y, w, h, rectRadius: o.radius === undefined ? 0.12 : o.radius,
    fill: { color: o.fill || C.mint },
    line: { color: o.line || o.fill || C.mint, width: o.lineWidth === undefined ? 0.75 : o.lineWidth },
    shadow: o.shadow ? { type: 'outer', blur: 6, offset: 2, angle: 90, color: '000000', opacity: 0.12 } : undefined,
  });
}

function arrow(slide, x1, y1, x2, y2, o = {}) {
  const x = Math.min(x1, x2), y = Math.min(y1, y2);
  const w = Math.abs(x2 - x1), h = Math.abs(y2 - y1);
  const line = { color: o.color || C.muted, width: o.width || 1.5, endArrowType: 'triangle' };
  if (o.both) line.beginArrowType = 'triangle';
  if (o.dash) line.dashType = o.dash;
  slide.addShape(pres.ShapeType.line, { x, y, w, h, flipH: x2 < x1, flipV: y2 < y1, line });
}

function edgeLabel(slide, str, x, y, w, h, o = {}) {
  text(slide, str, x, y, w, h, {
    size: o.size || 8, color: o.color || C.muted, align: 'center', valign: 'middle',
    fill: o.fill === null ? undefined : (o.fill || C.white), italic: !!o.italic,
  });
}

function title(slide, str, o = {}) {
  if (o.kicker) {
    text(slide, o.kicker.toUpperCase(), 0.6, 0.22, 9, 0.25, { size: 9, color: o.kickerColor || C.forest, charSpacing: 2, bold: true });
  }
  text(slide, str, 0.6, o.kicker ? 0.48 : 0.4, 12.1, 0.8, {
    font: F.head, size: o.size || 30, bold: true, color: o.color || C.ink, valign: 'middle',
  });
}

function footer(slide, n, dark = false) {
  text(slide, `Broccoli DevOps Agent   ${n}`, 9.3, 7.08, 3.4, 0.25, {
    size: 8, color: dark ? C.paleOnDark : C.muted, align: 'right',
  });
}

function box(slide, x, y, w, h, lines, o = {}) {
  card(slide, x, y, w, h, { fill: o.fill, line: o.line, radius: o.radius === undefined ? 0.08 : o.radius, lineWidth: o.lineWidth === undefined ? 1 : o.lineWidth });
  const arr = lines.map((l, i) => ({
    t: l.t, bold: !!l.bold, size: l.size || (i === 0 ? 11 : 8.5), color: l.color || o.color || C.text,
  }));
  runs(slide, arr, x + 0.08, y + 0.04, w - 0.16, h - 0.08, { align: o.align || 'center', valign: 'middle' });
}

function blank(bg) {
  const s = pres.addSlide();
  s.background = { color: bg || C.white };
  return s;
}

// ---------- slides ----------
async function build() {
  let n = 0;

  // 1. Title ---------------------------------------------------------------
  {
    const s = blank(C.ink); n++;
    text(s, 'THUSAAC  ·  AGENTIC OPERATIONS FOR THE BROCCOLI ONLINE JUDGE', 0.8, 1.55, 9, 0.3, { size: 10, color: C.moss, charSpacing: 2, bold: true });
    text(s, 'Broccoli DevOps Agent', 0.8, 1.95, 8.6, 1.3, { font: F.head, size: 50, bold: true, color: C.white, valign: 'middle' });
    text(s, 'An operations control plane that observes a judging deployment, lets a model investigate, and executes only what policy and a human allow.', 0.8, 3.35, 7.9, 1.1, { size: 19, color: C.paleOnDark, lineSpacing: 1.1 });
    text(s, '“The model proposes; the runtime records and enforces.”', 0.8, 4.75, 7.9, 0.5, { size: 16, italic: true, color: C.white });
    text(s, 'Rust control plane and agent harness  ·  web and terminal consoles  ·  v0.1, September 2026', 0.8, 6.55, 9, 0.35, { size: 11, color: C.paleOnDark });

    const pillars = [['LuEye', 'Observe'], ['LuBrainCircuit', 'Decide'], ['LuShieldCheck', 'Act safely']];
    let y = 2.05;
    for (const [ic, label] of pillars) {
      await iconCircle(s, ic, 10.2, y, 0.9, C.forest, C.white);
      text(s, label, 11.25, y + 0.2, 1.8, 0.5, { size: 16, bold: true, color: C.white, valign: 'middle' });
      y += 1.25;
    }
    s.addNotes('Opening: this is an agentic DevOps control plane for the Broccoli online judge, built in Rust. The one sentence to remember: the model proposes, the runtime records and enforces. Three verbs structure the talk: observe, decide, act safely.');
  }

  // 2. The problem ----------------------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'Contest day is an operations problem', { kicker: 'Why this exists' });
    const rows = [
      ['LuServer', 'Many hosts, many services', 'PostgreSQL, Redis, CephFS, the API server, the frontend, judge workers with sandboxes, printer and balloon stations. All of it must keep judging while hundreds of contestants submit.'],
      ['LuEyeOff', 'Failures are often invisible', 'A judge worker has no inbound port; its only honest signal is a heartbeat in Redis. A slow storage volume keeps every port open while latency climbs a hundredfold. Reachable is not healthy.'],
      ['LuTriangleAlert', 'Every fix is risky', 'Restart the wrong service, purge a queue, change a scoring key, and verdicts change. An assistant that acts must be bound by policy, not by prompt wording.'],
    ];
    let y = 1.55;
    for (const [ic, h, b] of rows) {
      await iconCircle(s, ic, 0.6, y + 0.05, 0.7, C.mint, C.forest);
      text(s, h, 1.55, y, 6.2, 0.4, { size: 16, bold: true, color: C.ink });
      text(s, b, 1.55, y + 0.42, 6.2, 1.1, { size: 11.5, color: C.text, lineSpacing: 1.05 });
      y += 1.72;
    }
    card(s, 8.5, 1.55, 4.25, 5.0, { fill: C.ink, line: C.ink });
    text(s, 'THE BRIEF', 8.85, 1.85, 3.6, 0.3, { size: 9, bold: true, color: C.moss, charSpacing: 2 });
    bullets(s, [
      'Harness and orchestration layer implemented in Rust (the course constraint).',
      'Useful during a real contest before it is useful as a course demonstration.',
      'Human reports outrank everything the automation finds.',
      'No model output is ever authoritative machine state.',
      'Machines are touched only through runbooks the operator configured.',
    ], 8.85, 2.3, 3.55, 4.0, { size: 12.5, color: C.white, after: 10 });
    s.addNotes('Set up the pain: distributed services, failures the probes cannot see, and the asymmetry between the cost of a wrong action and the number of people qualified to act. The brief box lists the constraints the design was built against.');
  }

  // 3. One control plane, three verbs -------------------------------------
  {
    const s = blank(); n++;
    title(s, 'One control plane, three verbs', { kicker: 'What it is' });
    text(s, 'It observes a deployment into immutable Snapshots, decides through a Scheduler that wraps a model in a deterministic harness, and acts only through approved runbooks that a human can always stop.', 0.6, 1.35, 12.1, 0.75, { size: 14, color: C.text, lineSpacing: 1.1 });
    const cols = [
      ['LuEye', 'Observe', C.forest, ['A static topology of hosts, endpoints, and dependencies.', 'Six read-only probes, including Broccoli\'s own admin API for worker heartbeats and queue depth.', 'One Snapshot at startup, one every two minutes, and on demand.', 'Unknown is explicit: every unprobed resource is a coverage gap.']],
      ['LuBrainCircuit', 'Decide', C.blue, ['Human reports open Issues at a priority reserved for humans.', 'The Top Scheduler consults a policy model only at fixed decision points and validates every answer.', 'An Operate Team investigates with six typed tools over one sanitized Snapshot View.', 'A bounded chain of passes: observe, act, check.']],
      ['LuShieldCheck', 'Act safely', C.red, ['A 26-class authority matrix per operation mode: auto, approve, or deny.', 'Runbook commands only, dry-run by default, credentials in the SSH agent.', 'Before and after Snapshots; graded verification evidence.', 'Everything a human must decide waits in a three-category inbox.']],
    ];
    let x = 0.6;
    for (const [ic, h, col, items] of cols) {
      card(s, x, 2.35, 3.9, 4.45, { fill: C.white, line: C.line, shadow: true });
      await iconCircle(s, ic, x + 0.3, 2.65, 0.75, col, C.white);
      text(s, h, x + 1.2, 2.72, 2.4, 0.6, { font: F.head, size: 22, bold: true, color: col, valign: 'middle' });
      bullets(s, items, x + 0.3, 3.6, 3.3, 3.0, { size: 12.5, after: 10 });
      x += 4.1;
    }
    s.addNotes('The elevator pitch. Each column is one path of the architecture on the next slide: observation, control, execution.');
  }

  // 4. Architecture ---------------------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'Architecture: three paths, one gateway', { kicker: 'System view' });
    // lane headers
    text(s, 'OBSERVATION PATH', 0.9, 1.4, 3.2, 0.25, { size: 8.5, bold: true, color: C.forest, charSpacing: 2 });
    text(s, 'CONTROL PATH', 4.6, 1.4, 3, 0.25, { size: 8.5, bold: true, color: C.blue, charSpacing: 2 });
    text(s, 'EXECUTION PATH', 9.7, 1.4, 3, 0.25, { size: 8.5, bold: true, color: C.red, charSpacing: 2 });

    // observation lane
    box(s, 0.9, 1.75, 1.5, 0.85, [{ t: 'Reporter', bold: true, size: 10.5 }, { t: 'status reports' }], { fill: C.mint, line: C.forest });
    box(s, 2.55, 1.75, 1.55, 0.85, [{ t: 'Snapshot Judge', bold: true, size: 10.5 }, { t: 'rules + LLM, Judge View' }], { fill: C.mint, line: C.forest });
    box(s, 0.9, 3.05, 3.2, 0.95, [{ t: 'Snapshot Store / AutoLog DB', bold: true }, { t: 'immutable Snapshots · append-only EventLog · Artifacts' }], { fill: C.mint, line: C.forest });
    box(s, 0.9, 4.45, 3.2, 0.95, [{ t: 'Collector', bold: true }, { t: 'probe registry · periodic capture · redaction' }], { fill: C.mint, line: C.forest });
    // control lane
    box(s, 5.2, 1.75, 7.55, 0.85, [
      { t: 'Humans  ·  web console · TUI · CLI', bold: true },
      { t: 'reports at HumanTop · approve / reject · acknowledge / send back upstream · freeze / resume · close' },
    ], { fill: C.amberTint, line: C.amber });
    box(s, 4.6, 3.05, 4.0, 1.1, [
      { t: 'Top Scheduler', bold: true, size: 12 },
      { t: 'deterministic harness around a policy model' },
      { t: 'Issues · Jobs · priorities · freeze · recovery' },
    ], { fill: C.blueTint, line: C.blue });
    box(s, 4.6, 4.75, 4.0, 0.95, [
      { t: 'Operate Team', bold: true },
      { t: 'harness-backed model with six typed tools, or read-only rules' },
      { t: 'one pass = one immutable Snapshot View' },
    ], { fill: C.blueTint, line: C.blue });
    // execution lane
    box(s, 9.7, 4.45, 3.05, 1.25, [
      { t: 'Agents Platform', bold: true },
      { t: 'joint-scope re-check · runbook commands' },
      { t: 'per-target execution lanes · dry-run by default' },
      { t: 'process-group kill on timeout' },
    ], { fill: C.redTint, line: C.red });
    // machines
    box(s, 3.2, 6.35, 7.0, 0.7, [
      { t: 'Machines and services', bold: true },
      { t: 'PostgreSQL · Redis · CephFS · API server · frontend · judge workers · stations' },
    ], { fill: C.gray, line: C.grayLine });

    // edges
    arrow(s, 3.6, 6.35, 2.6, 5.4); // machines -> collector
    edgeLabel(s, 'status · logs · heartbeats', 2.3, 5.72, 1.6, 0.26);
    arrow(s, 2.5, 4.45, 2.5, 4.0); // collector -> store
    arrow(s, 1.65, 3.05, 1.65, 2.6); // store -> reporter
    arrow(s, 3.3, 3.05, 3.3, 2.6); // store -> judge
    arrow(s, 4.1, 2.35, 4.6, 3.3, { color: C.forest }); // judge -> scheduler
    edgeLabel(s, 'issue candidates', 3.6, 2.66, 1.35, 0.26);
    arrow(s, 4.1, 3.5, 4.6, 3.5); // store -> scheduler
    edgeLabel(s, 'Snapshot', 4.08, 3.56, 0.55, 0.24, { fill: null });
    arrow(s, 4.6, 3.95, 4.1, 4.7, { color: C.blue }); // scheduler -> collector
    edgeLabel(s, 'capture · probes', 3.65, 4.2, 1.15, 0.26);
    arrow(s, 5.9, 2.6, 5.9, 3.05, { color: C.amber }); // humans -> scheduler
    edgeLabel(s, 'reports · decisions · feedback', 6.0, 2.68, 2.0, 0.26, { fill: null });
    arrow(s, 8.3, 3.05, 8.3, 2.6, { color: C.amber }); // scheduler -> humans
    edgeLabel(s, 'inbox: requests · denials · failures · progress', 8.4, 2.68, 2.9, 0.26, { fill: null });
    arrow(s, 5.4, 4.15, 5.4, 4.75, { color: C.blue }); // scheduler -> team
    arrow(s, 7.8, 4.75, 7.8, 4.15, { color: C.blue }); // team -> scheduler
    edgeLabel(s, '↓ sanitized View\n↑ diagnosis · proposals · probe request', 5.55, 4.2, 2.15, 0.48, { fill: null, size: 7.5 });
    arrow(s, 8.6, 5.2, 9.7, 5.2, { color: C.red }); // team -> platform (inspect)
    edgeLabel(s, 'inspect (read-only)', 8.62, 4.93, 1.06, 0.24, { fill: null });
    arrow(s, 8.6, 3.6, 11.0, 4.45, { color: C.red, both: true }); // scheduler <-> platform
    edgeLabel(s, 'ActionRun → · ← result, evidence', 8.95, 3.78, 1.9, 0.26);
    arrow(s, 10.9, 5.7, 9.9, 6.35, { color: C.red }); // platform -> machines
    edgeLabel(s, 'runbook over SSH', 9.75, 5.9, 1.25, 0.26);

    text(s, 'Only the Scheduler requests captures and creates ActionRuns, so freeze modes are enforced at one gate; the Platform re-checks every request anyway.', 0.6, 7.1, 8.6, 0.28, { size: 8.5, color: C.muted, italic: true });
    s.addNotes('Walk the three lanes. Left: machines are probed into immutable Snapshots that feed the store, the Judge, and the Reporter. Middle: humans and the Judge feed the Scheduler, which hands a Team one sanitized View and gets back proposals. Right: only the Scheduler turns a proposal into an ActionRun; the Platform is the one place with SSH reach. Point out the single control edge back into observation and the read-only inspect edge.');
  }

  // 5. Design principles ----------------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'Eight principles every later decision was checked against', { kicker: 'Design principles' });
    const items = [
      ['LuCamera', 'Snapshot-based reasoning', 'A Job reasons over one immutable Snapshot and never silently reads later state.'],
      ['LuLayers', 'Global coordination, scoped execution', 'The Scheduler owns priorities and Issues; a Team gets one sanitized View and one scope.'],
      ['LuLock', 'One execution gateway', 'Credentials and machine mutation live in the Agents Platform only.'],
      ['LuUsers', 'Human reports first', 'HumanTop priority is reserved for humans; model and Judge output is clamped below it.'],
      ['LuHistory', 'Immutable, replayable evidence', 'Snapshots, Views, transcripts, callbacks, actions, and events are kept for replay.'],
      ['LuGitBranch', 'Operate ≠ Develop', 'Operate Teams change deployed systems; Develop Teams change source in worktrees.'],
      ['LuShield', 'Model proposes, runtime enforces', 'IDs, transitions, permissions, execution, and verification belong to the Rust runtime.'],
      ['LuFileText', 'Untrusted input stays data', 'Logs, filenames, contestant strings, and report text are fenced in model context.'],
    ];
    const w = 2.8, h = 2.45, gap = 0.3;
    for (let i = 0; i < items.length; i++) {
      const [ic, hd, bd] = items[i];
      const x = 0.6 + (i % 4) * (w + gap), y = 1.5 + Math.floor(i / 4) * (h + 0.3);
      card(s, x, y, w, h, { fill: C.mint, line: C.mint });
      await iconCircle(s, ic, x + 0.25, y + 0.25, 0.6, C.forest, C.white);
      text(s, hd, x + 0.25, y + 1.0, w - 0.5, 0.5, { size: 13, bold: true, color: C.ink });
      text(s, bd, x + 0.25, y + 1.5, w - 0.5, 0.9, { size: 10.5, color: C.text, lineSpacing: 1.05 });
    }
    s.addNotes('These were agreed before implementation. Principles 4, 7 and 8 do most of the work in later slides: the HumanTop reservation, the harness-validates-model pattern, and fencing untrusted text.');
  }

  // 6. Observation -----------------------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'Observation: what the Collector can honestly say', { kicker: 'Observe' });
    const hdr = (t) => ({ text: t, options: { bold: true, color: C.white, fill: { color: C.forest }, fontSize: 10.5, fontFace: F.body } });
    const cell = (t, o = {}) => ({ text: t, options: { fontSize: 10, fontFace: F.body, color: C.text, ...o } });
    const rows = [
      [hdr('Probe'), hdr('Reads'), hdr('Health signal')],
      [cell('tcp.connect', { bold: true }), cell('reachability and latency of host:port'), cell('Degraded above a latency threshold')],
      [cell('http.status', { bold: true }), cell('status code of a plain HTTP GET'), cell('latency as above')],
      [cell('redis.llen', { bold: true }), cell('length of one Redis list (queue backlog)'), cell('Degraded outside [min, max]')],
      [cell('http.json', { bold: true }), cell('one value at a JSON pointer'), cell('exact expect, or a numeric range')],
      [cell('broccoli.worker', { bold: true }), cell('a worker\'s heartbeat from Broccoli\'s admin API'), cell('Healthy on a live heartbeat, Degraded when stale, Down when absent')],
      [cell('broccoli.queue', { bold: true }), cell('one queue\'s depth from the admin overview'), cell('Degraded outside [min, max]; in-progress and dead-letter counts')],
    ];
    s.addTable(rows, {
      x: 0.6, y: 1.5, w: 7.7, colW: [1.5, 3.0, 3.2], rowH: 0.52,
      border: { type: 'solid', color: C.line, pt: 0.75 }, fill: { color: C.white }, valign: 'middle', margin: 0.06,
    });
    text(s, 'Six read-only probes, configured per resource in the topology file. The Broccoli probes read the same data as the admin dashboard, with a login the file only names.', 0.6, 5.35, 7.7, 0.7, { size: 10.5, color: C.muted, italic: true });

    card(s, 8.7, 1.5, 4.05, 2.55, { fill: C.mint, line: C.mint });
    await iconCircle(s, 'LuCircleHelp', 8.95, 1.75, 0.6, C.forest, C.white);
    text(s, 'Unknown is not Healthy', 9.7, 1.8, 2.9, 0.5, { size: 14, bold: true, color: C.ink, valign: 'middle' });
    text(s, 'A resource without a probe is Unknown with an explicit coverage gap. A judge worker has no inbound port, so its heartbeat is the only honest observation; without it the agent says "I cannot see it" rather than guessing.', 8.95, 2.5, 3.55, 1.5, { size: 11, lineSpacing: 1.05 });

    card(s, 8.7, 4.25, 4.05, 2.55, { fill: C.mint, line: C.mint });
    await iconCircle(s, 'LuCamera', 8.95, 4.5, 0.6, C.forest, C.white);
    text(s, 'Snapshots are immutable', 9.7, 4.55, 2.9, 0.5, { size: 14, bold: true, color: C.ink, valign: 'middle' });
    text(s, 'One at startup, one every two minutes in every mode, and on demand from the Scheduler: after a report, when a Team asks for probes, before and after an action. Each records freshness, trust, and gaps, and is replayable.', 8.95, 5.25, 3.55, 1.5, { size: 11, lineSpacing: 1.05 });
    s.addNotes('The observation story is about honesty rather than coverage: the two Broccoli probes were added because a worker has no port, and the coverage gap is the designed answer when a probe is missing. Snapshots are immutable so every later decision can be replayed against exactly what was seen.');
  }

  // 7. Top Scheduler ----------------------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'The Top Scheduler: a deterministic harness around a policy model', { kicker: 'Decide', size: 28 });
    const steps = [
      ['Input', 'snapshot · issue candidate · human report · team callback · human choice · timer'],
      ['Deterministic pre-checks', 'mode gates · state machines · dedup keys · scope'],
      ['Policy consultation', 'only at fixed decision points, typed request and response'],
      ['Harness validation', 'clamp priorities · verify references and transitions · check conflicts'],
      ['Apply + append events', 'model input and output stored for exact replay'],
    ];
    const w = 2.2, gap = 0.28;
    for (let i = 0; i < steps.length; i++) {
      const x = 0.6 + i * (w + gap);
      const fill = i === 2 ? C.blueTint : C.mint, line = i === 2 ? C.blue : C.forest;
      card(s, x, 1.55, w, 1.55, { fill, line, radius: 0.1 });
      text(s, String(i + 1), x + 0.15, 1.65, 0.4, 0.35, { size: 12, bold: true, color: line });
      text(s, steps[i][0], x + 0.15, 1.98, w - 0.3, 0.4, { size: 12.5, bold: true, color: C.ink });
      text(s, steps[i][1], x + 0.15, 2.38, w - 0.3, 0.7, { size: 9.5, color: C.text, lineSpacing: 1.05 });
      if (i < steps.length - 1) arrow(s, x + w + 0.03, 2.32, x + w + gap - 0.03, 2.32, { color: C.muted, width: 1.25 });
    }
    card(s, 0.6, 3.45, 5.9, 3.4, { fill: C.white, line: C.line, shadow: true });
    await iconCircle(s, 'LuWaypoints', 0.9, 3.7, 0.6, C.blue, C.white);
    text(s, 'Consulted only at fixed decision points', 1.65, 3.75, 4.6, 0.5, { size: 14, bold: true, color: C.ink, valign: 'middle' });
    bullets(s, [
      'Candidate triage: accept, merge into an open Issue, or reject, with a proposed priority (clamped below HumanTop).',
      'Callback interpretation: resnapshot with specific probes, convert proposals into ActionRuns, ask a human, resolve, or give up. Probe lists and proposal indexes are verified against the real JobResult.',
      'Every consultation is an event with its exact input and output, so post-contest review can audit each model-influenced decision.',
    ], 0.9, 4.4, 5.3, 2.3, { size: 12, after: 9 });
    card(s, 6.85, 3.45, 5.9, 3.4, { fill: C.ink, line: C.ink });
    await iconCircle(s, 'LuBan', 7.15, 3.7, 0.6, C.red, C.white);
    text(s, 'Never routed through the model', 7.9, 3.75, 4.6, 0.5, { size: 14, bold: true, color: C.white, valign: 'middle' });
    bullets(s, [
      'Freeze modes, approval gates, and state-machine transitions.',
      'Capability and target scoping; the HumanTop reservation; idempotency.',
      'When the model is down, every decision point degrades to a conservative default: candidates are recorded for a human, next steps become a question. Collection, persistence, dispatch bookkeeping, and recovery continue.',
    ], 7.15, 4.4, 5.3, 2.3, { size: 12, color: C.white, after: 9 });
    s.addNotes('This is the AI-integrated Scheduler pattern: the model handles judgment calls but only as proposals inside a deterministic loop. Today the policy adapter is not wired, so every decision point runs the fallback, deliberately, so that path was the first one exercised.');
  }

  // 8. Operate Team + harness ------------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'The Operate Team and the harness underneath it', { kicker: 'Decide' });
    text(s, 'SIX TYPED TOOLS PER PASS', 0.6, 1.4, 5, 0.25, { size: 9, bold: true, color: C.blue, charSpacing: 2 });
    const tools = [
      ['read_snapshot_view', 'the sanitized View, fenced as untrusted data'],
      ['inspect', 'a non-mutating runbook on an in-scope target, through the Platform'],
      ['request_probes', 'ends the pass; a fresh Snapshot with those probes starts the next'],
      ['report_progress', 'a progress line for the consoles'],
      ['propose_action', 'runbook, targets, arguments, reason, expected effect, verification probes'],
      ['submit_diagnosis', 'diagnosis_only or solved; may ask for a follow-up pass'],
    ];
    for (let i = 0; i < tools.length; i++) {
      const x = 0.6 + (i % 2) * 3.15, y = 1.75 + Math.floor(i / 2) * 1.35;
      card(s, x, y, 2.95, 1.15, { fill: C.blueTint, line: C.blueTint, radius: 0.1 });
      text(s, tools[i][0], x + 0.2, y + 0.12, 2.6, 0.35, { size: 12, bold: true, color: C.blue, font: 'Courier New' });
      text(s, tools[i][1], x + 0.2, y + 0.5, 2.6, 0.6, { size: 9.5, color: C.text, lineSpacing: 1.05 });
    }
    text(s, 'A second backend, deterministic and read-only, diagnoses from the View and never proposes. The Scheduler cannot tell the two apart.', 0.6, 5.95, 6.1, 0.8, { size: 10.5, color: C.muted, italic: true, lineSpacing: 1.05 });

    card(s, 7.05, 1.4, 5.7, 5.45, { fill: C.ink, line: C.ink });
    await iconCircle(s, 'LuCpu', 7.35, 1.65, 0.6, C.moss, C.ink);
    text(s, 'crates/harness', 8.1, 1.7, 4.4, 0.5, { size: 15, bold: true, color: C.white, valign: 'middle', font: 'Courier New' });
    bullets(s, [
      'A model-agnostic agent loop, generic over its ModelClient boundary; the OpenAI-compatible relay client is one feature.',
      'Typed, allowlisted tools; terminal tools for structured output.',
      'Turn, tool-call, and token budgets with a low-budget warning and wrap-up turns that offer only the terminal tools.',
      'Retries with backoff on transient backend failures.',
      'Cooperative cancellation at step boundaries.',
      'Replayable transcripts with one record per model turn: latency, tokens, retries, tools on offer.',
      'A progress observer that sees every entry the moment it is appended.',
    ], 7.35, 2.35, 5.1, 3.7, { size: 12, color: C.white, after: 8 });
    text(s, 'Knows nothing about Broccoli. The control plane depends on it; never the reverse.', 7.35, 6.15, 5.1, 0.5, { size: 10.5, italic: true, color: C.moss });
    s.addNotes('The tools are the whole surface a model has. Every effect of a tool is validated by the Scheduler or the Platform, never trusted. The harness crate is our own: it is what the course asked for in Rust, and it is generic enough that a codex-backed Team could implement the same port.');
  }

  // 9. Investigation loop ---------------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'The investigation loop: observe, act, check', { kicker: 'Decide' });
    const passes = [
      ['Pass 1 · observe', 'reads the View, inspects read-only, asks for the probes it lacks', C.mint, C.forest],
      ['Pass 2 · act', 'proposes runbook actions; the matrix decides; the Platform executes and verifies', C.blueTint, C.blue],
      ['Pass 3 · check', 'reads the after-Snapshot and the execution evidence; concludes or asks a human', C.mint, C.forest],
    ];
    const pw = 3.3, gap = 1.1;
    for (let i = 0; i < passes.length; i++) {
      const x = 0.6 + i * (pw + gap);
      card(s, x, 1.55, pw, 1.5, { fill: passes[i][2], line: passes[i][3], radius: 0.1 });
      text(s, passes[i][0], x + 0.2, 1.65, pw - 0.4, 0.4, { size: 15, bold: true, color: C.ink });
      text(s, passes[i][1], x + 0.2, 2.08, pw - 0.4, 0.9, { size: 10.5, lineSpacing: 1.05 });
      if (i < 2) {
        arrow(s, x + pw + 0.05, 2.3, x + pw + gap - 0.05, 2.3, { color: C.muted });
        edgeLabel(s, i === 0 ? 'request_probes →\nsuperseding pass over\na fresh Snapshot' : 'follow_up →\npass over the\nafter-Snapshot', x + pw + 0.05, 2.5, gap - 0.1, 0.6, { size: 7.5, fill: null });
      }
    }
    // human loop
    card(s, 0.6, 3.45, 12.1, 0.75, { fill: C.amberTint, line: C.amber, radius: 0.1 });
    await iconCircle(s, 'LuInbox', 0.8, 3.55, 0.55, C.amber, C.white);
    runs(s, [
      { t: 'Whenever something waits for a human, the chain stops. ', bold: true, size: 11.5 },
      { t: 'A held action resumes it on approval. A denial or failure that is sent back upstream starts a revising pass over a fresh Snapshot, with the reason, the evidence, and the operator\'s comment in its View.', size: 11.5, brk: false },
    ], 1.5, 3.5, 11.0, 0.65, { valign: 'middle' });
    const rules = [
      ['LuGauge', 'Pass budget', 'max_auto_passes (default 3) per report or send-back. At zero the probe tool is not offered; a Team that still asks stalls in the Failed inbox.'],
      ['LuCamera', 'One pass, one Snapshot', 'No probe request after a proposal in the same pass; no "solved" while proposals are pending. Act now, or gather more evidence.'],
      ['LuListChecks', 'Follow-up after settlement', 'A follow-up pass runs only once every proposal executed or was denied; nothing is re-observed when nothing ran.'],
      ['LuBadgeCheck', '"Solved" is a claim', 'Accepted only with Weak or Strong evidence, never dry-run, and every touched resource Healthy in the pass\'s own Snapshot. Otherwise: DiagnosisOnly, a human decides.'],
    ];
    const rw = 2.8, rgap = 0.3;
    for (let i = 0; i < rules.length; i++) {
      const x = 0.6 + i * (rw + rgap);
      card(s, x, 4.45, rw, 2.4, { fill: C.white, line: C.line, shadow: true });
      await iconCircle(s, rules[i][0], x + 0.2, 4.65, 0.5, C.forest, C.white);
      text(s, rules[i][1], x + 0.8, 4.68, rw - 1.0, 0.45, { size: 12, bold: true, color: C.ink, valign: 'middle' });
      text(s, rules[i][2], x + 0.2, 5.25, rw - 0.4, 1.5, { size: 10, lineSpacing: 1.05 });
    }
    s.addNotes('Harder situations are handled by chaining passes, never by letting a running Job read live state. The four rules at the bottom are enforced by the runtime, not requested from the model; the Solved clamp is why a dry-run rehearsal can never resolve an Issue.');
  }

  // 10. Authority + inbox -----------------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'Authority: the matrix decides, the inbox holds the rest', { kicker: 'Act safely' });
    const hdr = (t) => ({ text: t, options: { bold: true, color: C.white, fill: { color: C.ink }, fontSize: 10, fontFace: F.body, align: 'center' } });
    const name = (t) => ({ text: t, options: { fontSize: 10, fontFace: F.body, color: C.text } });
    const dec = (t) => {
      const map = { auto: [C.mint, C.forest], approve: [C.amberTint, '8A5A12'], deny: [C.redTint, C.red], 'human-only': [C.gray, C.muted] };
      const [fill, color] = map[t];
      return { text: t, options: { fontSize: 10, bold: true, fontFace: F.body, color, fill: { color: fill }, align: 'center' } };
    };
    const rows = [
      [hdr('Operation class (examples)'), hdr('rehearsal'), hdr('contest_locked'), hdr('post_contest')],
      [name('Observe: probes, log reads, status'), dec('auto'), dec('auto'), dec('auto')],
      [name('Restart a judge worker'), dec('auto'), dec('auto'), dec('auto')],
      [name('Restart the API server'), dec('auto'), dec('approve'), dec('auto')],
      [name('Purge a queue'), dec('approve'), dec('deny'), dec('approve')],
      [name('Change a contest-affecting config key'), dec('approve'), dec('deny'), dec('approve')],
      [name('Deploy a release Bundle'), dec('approve'), dec('deny'), dec('approve')],
      [name('Reboot a machine'), dec('approve'), dec('approve'), dec('approve')],
      [name('Free-form shell command'), dec('deny'), dec('deny'), dec('deny')],
      [name('Change the operation mode'), dec('human-only'), dec('human-only'), dec('human-only')],
    ];
    s.addTable(rows, {
      x: 0.6, y: 1.5, w: 7.0, colW: [3.1, 1.2, 1.5, 1.2], rowH: 0.4,
      border: { type: 'solid', color: C.line, pt: 0.75 }, fill: { color: C.white }, valign: 'middle', margin: 0.05,
    });
    text(s, '26 operation classes × 3 modes, approved by the operator and encoded in src/policy.rs. On top: deny is never approvable, a repeated automatic action escalates to approval, a duplicate of a live action is denied by its idempotency key, and every target\'s kind, the Job\'s scope, and the arguments are checked before the row applies.', 0.6, 5.65, 7.0, 1.2, { size: 10, color: C.muted, italic: true, lineSpacing: 1.05 });

    const cats = [
      ['LuHand', 'Permission Request', 'actions on an approve row', 'approve · reject with a comment', C.amber],
      ['LuOctagonX', 'Permission Denied', 'rule denials with the rationale · human rejections with the comment', 'acknowledge · send back upstream', C.red],
      ['LuBug', 'Failed', 'failed Jobs · failed executions · failed verifications · stalled passes', 'acknowledge · send back upstream', C.muted],
    ];
    let y = 1.5;
    for (const [ic, hd, what, dec2, col] of cats) {
      card(s, 8.0, y, 4.75, 1.6, { fill: C.white, line: C.line, shadow: true });
      await iconCircle(s, ic, 8.2, y + 0.2, 0.55, col, C.white);
      text(s, hd, 8.9, y + 0.22, 3.7, 0.45, { size: 13.5, bold: true, color: C.ink, valign: 'middle' });
      text(s, what, 8.9, y + 0.68, 3.7, 0.5, { size: 9.5, color: C.text, lineSpacing: 1.05 });
      runs(s, [{ t: 'decisions: ', bold: true, size: 9.5, color: col }, { t: dec2, size: 9.5, brk: false }], 8.9, y + 1.2, 3.7, 0.3);
      y += 1.75;
    }
    text(s, 'Send back upstream = a revising pass under the same Issue with the human\'s feedback in its View. The Team must visibly use it.', 8.0, 6.66, 4.75, 0.4, { size: 9, color: C.muted, italic: true, lineSpacing: 1.0 });
    s.addNotes('The matrix is deliberately conservative: during a live contest the default answer is "ask a human" and anything that could change verdicts is denied outright. One inbox, three categories, two decisions each; the send-back is the human-in-the-loop mechanism.');
  }

  // 11. Execution safety ------------------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'Execution safety: between "approved" and "verified"', { kicker: 'Act safely' });
    const items = [
      ['LuScanSearch', 'Joint scope authorization', 'Runbook, every target\'s kind, the Job\'s target scope and capabilities, and the arguments are checked by the Scheduler, then re-checked by the Platform. A worker restart cannot be pointed at the API server.'],
      ['LuLock', 'Compare-and-set and idempotency', 'Every transition writes only if the record still equals what the caller read. The idempotency key is claimed atomically: two operators, or a retry racing its original, cannot both apply.'],
      ['LuTimerOff', 'Process-group kill on timeout', 'Each command runs in its own process group with a wall-clock limit. On timeout the whole group is killed and reaped before the failure is reported, so "failed" never means "still running".'],
      ['LuFlaskConical', 'Dry-run by default', 'Commands are rendered and recorded as Artifacts, not executed, until the operator sets dry_run = false. A rehearsal is an ActionRun that passes, labelled as no evidence of remediation.'],
      ['LuBadgeCheck', 'Graded verification', 'Mutating classes must show their effect in the after-Snapshot. Evidence is strong when the postcondition changed, weak when the target was already healthy, dry_run when nothing ran. Only real evidence resolves an Issue.'],
      ['LuSnowflake', 'Freeze and recovery', 'Dispatch-frozen or fully frozen, checked at the one gate. serve recovers first: interrupted Jobs and actions are reconciled into the inbox and the previous freeze is restored; dispatch resumes only after a clean restart.'],
    ];
    const w = 3.85, h = 2.45, gx = 0.28, gy = 0.3;
    for (let i = 0; i < items.length; i++) {
      const x = 0.6 + (i % 3) * (w + gx), y = 1.5 + Math.floor(i / 3) * (h + gy);
      card(s, x, y, w, h, { fill: i % 2 === 0 ? C.mint : C.redTint, line: i % 2 === 0 ? C.mint : C.redTint });
      await iconCircle(s, items[i][0], x + 0.25, y + 0.25, 0.55, i % 2 === 0 ? C.forest : C.red, C.white);
      text(s, items[i][1], x + 0.95, y + 0.27, w - 1.15, 0.5, { size: 13, bold: true, color: C.ink, valign: 'middle' });
      text(s, items[i][2], x + 0.25, y + 0.95, w - 0.5, 1.45, { size: 11, lineSpacing: 1.05 });
    }
    s.addNotes('These came out of the two external design reviews. Together they make an ActionRun something that is never left half-done: it is validated twice, applied once, killed if it overruns, verified against observation, and reconciled after a crash.');
  }

  // 12. Consoles ----------------------------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'What the operator sees: one API, three clients', { kicker: 'Operator interfaces' });
    // console mock
    const mx = 0.6, my = 1.5, mw = 6.9, mh = 5.3;
    card(s, mx, my, mw, mh, { fill: C.white, line: C.line, shadow: true, radius: 0.1 });
    s.addShape(pres.ShapeType.roundRect, { x: mx, y: my, w: 1.7, h: mh, rectRadius: 0.1, fill: { color: C.ink }, line: { color: C.ink, width: 0 } });
    text(s, 'Broccoli DevOps', mx + 0.2, my + 0.2, 1.4, 0.3, { size: 10, bold: true, color: C.white });
    const nav = ['Overview', 'Inbox', 'Issues & jobs', 'Trace', 'Events', 'Spend', 'Settings'];
    nav.forEach((t, i) => {
      const y = my + 0.7 + i * 0.42;
      if (i === 1) s.addShape(pres.ShapeType.roundRect, { x: mx + 0.12, y: y - 0.05, w: 1.46, h: 0.34, rectRadius: 0.05, fill: { color: C.forest }, line: { color: C.forest, width: 0 } });
      text(s, t, mx + 0.25, y, 1.3, 0.25, { size: 9.5, color: i === 1 ? C.white : C.paleOnDark, valign: 'middle' });
    });
    text(s, 'EN · 简体中文', mx + 0.2, my + mh - 0.45, 1.4, 0.3, { size: 8, color: C.paleOnDark });
    const cx = mx + 1.9;
    text(s, 'Inbox', cx, my + 0.2, 3, 0.35, { size: 13, bold: true, color: C.ink });
    text(s, 'frozen: no · snapshot 42 s ago · spend $1.84 / $20.00', cx, my + 0.55, 4.8, 0.25, { size: 8, color: C.muted });
    const tiles = [['1', 'permission request', C.amber], ['1', 'permission denied', C.red], ['2', 'failed', C.muted]];
    tiles.forEach(([num, lab, col], i) => {
      const x = cx + i * 1.62;
      card(s, x, my + 0.95, 1.5, 0.85, { fill: C.gray, line: C.gray, radius: 0.06 });
      text(s, num, x + 0.15, my + 1.0, 0.6, 0.5, { size: 20, bold: true, color: col, valign: 'middle' });
      text(s, lab, x + 0.15, my + 1.48, 1.3, 0.25, { size: 7.5, color: C.muted });
    });
    const items = [
      ['mq.purge on redis-mq', 'waiting for approval · rehearsal row: approve', C.amber, 'Approve   Reject'],
      ['mode.set on broccoli-server', 'denied by rule 26: the Job lacks operate.mode', C.red, 'Acknowledge   Send back'],
      ['worker.start on worker-1', 'failed: no command configured for the runbook', C.muted, 'Acknowledge   Send back'],
      ['pass 01a0…b60c6', 'failed: the model ended without submit_diagnosis', C.muted, 'Acknowledge   Send back'],
    ];
    items.forEach(([a, b, col, btn], i) => {
      const y = my + 2.0 + i * 0.78;
      card(s, cx, y, 4.75, 0.68, { fill: C.white, line: C.line, radius: 0.05 });
      s.addShape(pres.ShapeType.ellipse, { x: cx + 0.12, y: y + 0.24, w: 0.2, h: 0.2, fill: { color: col }, line: { color: col, width: 0 } });
      text(s, a, cx + 0.42, y + 0.08, 2.9, 0.25, { size: 9, bold: true, color: C.ink });
      text(s, b, cx + 0.42, y + 0.34, 3.0, 0.25, { size: 7.5, color: C.muted });
      text(s, btn, cx + 3.35, y + 0.2, 1.35, 0.28, { size: 7.5, bold: true, color: C.forest, align: 'right' });
    });
    text(s, 'Web console: React + Vite + Tailwind, Broccoli\'s own design tokens, no Broccoli code.', mx, my + mh + 0.08, mw, 0.3, { size: 9, color: C.muted, italic: true });

    const feats = [
      ['LuInbox', 'The inbox at the centre', 'three categories, two decisions each; every decision records the operator\'s name'],
      ['LuActivity', 'Live Snapshot and events', 'coverage gaps, freeze and resume, progress lines while a pass runs, Interrupt per pass'],
      ['LuFileSearch', 'Issues, passes, Trace', 'the pass chain and the transcript entry by entry, growing live; Export and Import sessions'],
      ['LuSettings2', 'Settings configurator', 'live values apply at once; policy values only while frozen, in your name; startup values read-only'],
      ['LuCoins', 'Spend', 'tokens and cost per Job and in total, against the ceiling'],
      ['LuTerminal', 'TUI and CLI', 'the same operations from a terminal: approve, reject, send back, interrupt, freeze'],
    ];
    let y = 1.5;
    for (const [ic, hd, bd] of feats) {
      await iconCircle(s, ic, 7.85, y + 0.02, 0.5, C.mint, C.forest);
      text(s, hd, 8.5, y, 4.2, 0.3, { size: 12, bold: true, color: C.ink });
      text(s, bd, 8.5, y + 0.3, 4.2, 0.55, { size: 9.5, color: C.text, lineSpacing: 1.05 });
      y += 0.9;
    }
    s.addNotes('Every console action is an API call onto an existing runner operation, so the authority matrix and the event log apply to a click exactly as to a CLI command. The mock on the left is the seeded demo state from the Quickstart: one item of each kind. The agent\'s output language is a startup setting; the console language switches at runtime.');
  }

  // 13. R4 / R5 / R6 ------------------------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'Graded requirements: all three shipped', { kicker: 'Course requirements' });
    const cols = [
      ['R4', 'LuPlay', 'Real-time progress and interruption', [
        'Every model turn, tool call, retry, and budget wrap-up is reported the moment it happens, in one ordered SSE stream with the model\'s own progress lines.',
        'A running pass is registered by Job ID: Interrupt in the web console, c in the TUI, POST /api/jobs/{id}/cancel, Ctrl-C on the CLI.',
        'Cancellation is cooperative: the Team stops at its next step, still delivers a final callback, keeps the transcript, and lands in the Failed inbox.',
      ]],
      ['R5', 'LuHistory', 'Context history: past tasks, save and load, trace', [
        'Issues & jobs is the history: every Issue with every pass, filterable and searchable.',
        'Trace shows the pass chain and the transcript entry by entry, live while it runs and identical when stored: inputs, turns with latency and tokens, tool calls, notices, results, decisions, events.',
        'Export writes a session (Issue, passes, Views, actions, Snapshots, events) as one JSON file, hashes verified; Import loads it as a read-only archive every control decision ignores.',
      ]],
      ['R6', 'LuCoins', 'Token and cost tracking with a halting budget', [
        'Every response\'s usage block is parsed in both wire formats and recorded per Job and as model.usage events; totals come from the log.',
        'Costs are derived on demand from the counts and the price list, so a changed price re-prices history. Unknown usage is counted as unknown, never as free.',
        'max_tokens_per_run stops a pass through the same wrap-up path as the turn budget; the deployment ceiling freezes the Scheduler and survives restarts.',
      ]],
    ];
    let x = 0.6;
    for (const [tag, ic, hd, items] of cols) {
      card(s, x, 1.5, 3.9, 5.35, { fill: C.white, line: C.line, shadow: true });
      text(s, tag, x + 0.3, 1.7, 1.2, 0.7, { font: F.head, size: 34, bold: true, color: C.forest, valign: 'middle' });
      await iconCircle(s, ic, x + 3.0, 1.75, 0.6, C.mint, C.forest);
      text(s, hd, x + 0.3, 2.45, 3.3, 0.7, { size: 13.5, bold: true, color: C.ink, lineSpacing: 1.05 });
      bullets(s, items, x + 0.3, 3.2, 3.3, 3.5, { size: 11.5, after: 10 });
      x += 4.1;
    }
    s.addNotes('These three are the graded acceptance criteria. R4 rides on the existing callback-to-event-to-SSE path; R5 defines a session as an Issue with its whole pass chain and keeps imports inert by filtering, not by state; R6 never stores money, only counts, so pricing can change after the fact.');
  }

  // 14. Testbed ----------------------------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'Testbed: three real hosts, faults with known ground truth', { kicker: 'Evaluation', size: 28 });
    const vms = [
      ['LuDatabase', 'infra-1', 'PostgreSQL 17 · Redis 7 · SeaweedFS'],
      ['LuServer', 'app-1', 'broccoli-server · web frontend · image builder'],
      ['LuCpu', 'judge-1', 'one judge worker with isolate'],
    ];
    let y = 1.5;
    for (const [ic, hd, bd] of vms) {
      card(s, 0.6, y, 4.2, 1.15, { fill: C.mint, line: C.mint });
      await iconCircle(s, ic, 0.85, y + 0.28, 0.6, C.forest, C.white);
      text(s, hd, 1.65, y + 0.2, 3.0, 0.4, { size: 14, bold: true, color: C.ink, font: 'Courier New' });
      text(s, bd, 1.65, y + 0.6, 3.0, 0.45, { size: 10, color: C.text });
      y += 1.3;
    }
    card(s, 0.6, 5.4, 4.2, 1.45, { fill: C.ink, line: C.ink });
    await iconCircle(s, 'LuMonitor', 0.85, 5.65, 0.6, C.moss, C.ink);
    text(s, 'Controller on the Mac', 1.65, 5.6, 3.0, 0.35, { size: 12.5, bold: true, color: C.white });
    text(s, 'OrbStack Linux VMs, one Docker engine each. dry_run = false: ActionRuns really restart containers through testbed/runbook.sh.', 1.65, 5.95, 3.0, 0.85, { size: 9.5, color: C.paleOnDark, lineSpacing: 1.05 });

    const hdr = (t) => ({ text: t, options: { bold: true, color: C.white, fill: { color: C.forest }, fontSize: 10, fontFace: F.body } });
    const c = (t, o = {}) => ({ text: t, options: { fontSize: 9.5, fontFace: F.body, color: C.text, ...o } });
    const rows = [
      [hdr('Scenario'), hdr('What the probes see'), hdr('What the agent must work out')],
      [c('stop-service.sh redis-mq', { bold: true }), c('redis-mq Down'), c('which dependents are affected, and in what order to recover')],
      [c('stop-service.sh broccoli-server', { bold: true }), c('server and frontend Down'), c('whether the cause is the server or something under it')],
      [c('stop-service.sh worker-1', { bold: true }), c('nothing, without the heartbeat probe'), c('the failure is invisible; a coverage gap must be said, not guessed')],
      [c('partition.sh app-1 infra-1', { bold: true }), c('server Down, infra all Healthy'), c('the link between two hosts, not either host')],
      [c('slow-storage.sh', { bold: true }), c('everything Healthy, latency 100×'), c('latency, not availability')],
    ];
    s.addTable(rows, {
      x: 5.1, y: 1.5, w: 7.65, colW: [2.3, 2.35, 3.0], rowH: 0.62,
      border: { type: 'solid', color: C.line, pt: 0.75 }, fill: { color: C.white }, valign: 'middle', margin: 0.06,
    });
    text(s, 'The scenarios differ in what the Collector can and cannot see, which is the point: two of them should fail to show up in a Snapshot, and the agent is supposed to say so rather than guess.', 5.1, 5.85, 7.65, 0.8, { size: 10.5, color: C.muted, italic: true, lineSpacing: 1.05 });
    s.addNotes('The testbed is where dry-run is off. Point out the worker scenario: before the Broccoli admin-API probe existed, a stopped worker was invisible by design, and the correct answer was a coverage gap.');
  }

  // 15. By the numbers + timeline ---------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'By the numbers', { kicker: 'Status, 2026-09-06' });
    const stats = [
      ['27k', 'lines of Rust', 'control plane, harness, TUI, tests'],
      ['4.2k', 'lines of TypeScript', 'the web console'],
      ['105', 'tests passing', 'control plane and harness, 13 suites'],
      ['26 × 3', 'authority matrix', 'operation classes × operation modes'],
      ['6 · 6 · 3', 'probes · tools · inbox categories', 'the whole model-facing surface'],
      ['18', 'commits in 8 days', '2026-08-29 to 2026-09-05'],
    ];
    const w = 1.9, gap = 0.14;
    for (let i = 0; i < stats.length; i++) {
      const x = 0.6 + i * (w + gap);
      card(s, x, 1.5, w, 1.75, { fill: i % 2 === 0 ? C.mint : C.white, line: i % 2 === 0 ? C.mint : C.line });
      text(s, stats[i][0], x + 0.15, 1.6, w - 0.3, 0.75, { font: F.head, size: 30, bold: true, color: C.forest, valign: 'middle' });
      text(s, stats[i][1], x + 0.15, 2.33, w - 0.3, 0.42, { size: 9.5, bold: true, color: C.ink, lineSpacing: 1.0 });
      text(s, stats[i][2], x + 0.15, 2.78, w - 0.3, 0.45, { size: 8.5, color: C.muted, lineSpacing: 1.05 });
    }
    // timeline
    text(s, 'TIMELINE', 0.6, 3.75, 3, 0.25, { size: 9, bold: true, color: C.forest, charSpacing: 2 });
    const ty = 4.6;
    s.addShape(pres.ShapeType.line, { x: 0.9, y: ty, w: 11.6, h: 0, line: { color: C.line, width: 3 } });
    const ms = [
      ['08-29', 'Schemas and the first architecture draft', 'checked against the operator\'s own diagram'],
      ['08-30', 'v0.1 vertical slice', 'topology, probes, Snapshots, reports, recovery; the harness crate and the harness-backed Team'],
      ['09-03', 'Relay, matrix, consoles', 'GPT relay backend; the authority matrix encoded and the ActionRun path opened; HTTP/SSE API with web and TUI'],
      ['09-05', 'Two review rounds applied', 'three-category inbox and feedback loop; joint scope, CAS, process-group kill, recovery; admin-API probes; zh-CN; cost budget; live progress; the investigation loop'],
      ['working tree', 'Context history and configurator', 'live trace, session export and import, Settings with three value classes, periodic capture'],
    ];
    const step = 11.6 / (ms.length - 1);
    const dw = 2.0;
    for (let i = 0; i < ms.length; i++) {
      const cx = 0.9 + i * step;
      s.addShape(pres.ShapeType.ellipse, { x: cx - 0.14, y: ty - 0.14, w: 0.28, h: 0.28, fill: { color: i === ms.length - 1 ? C.amber : C.forest }, line: { color: C.white, width: 2 } });
      const bx = Math.min(Math.max(cx - dw / 2, 0.6), 12.73 - dw);
      text(s, ms[i][0], bx, ty - 0.6, dw, 0.35, { size: 11, bold: true, color: i === ms.length - 1 ? C.amber : C.forest, align: 'center' });
      text(s, ms[i][1], bx, ty + 0.3, dw, 0.5, { size: 10.5, bold: true, color: C.ink, align: 'center', lineSpacing: 1.0 });
      text(s, ms[i][2], bx, ty + 0.85, dw, 1.7, { size: 8.5, color: C.muted, align: 'center', lineSpacing: 1.05 });
    }
    s.addNotes('Numbers are from the working tree on 2026-09-06: line counts include tests; 105 tests pass in the control plane and harness crates. The timeline shows how much of the design landed after the two external review rounds.');
  }

  // 16. Boundaries and roadmap -----------------------------------------------
  {
    const s = blank(); n++;
    title(s, 'Deliberate boundaries, and the next slices', { kicker: 'Roadmap' });
    card(s, 0.6, 1.5, 5.9, 5.35, { fill: C.mint, line: C.mint });
    await iconCircle(s, 'LuSquare', 0.9, 1.75, 0.55, C.forest, C.white);
    text(s, 'Not in v0.1, on purpose', 1.6, 1.78, 4.6, 0.5, { size: 15, bold: true, color: C.ink, valign: 'middle' });
    bullets(s, [
      'Model-backed Scheduler decisions: the policy and Judge adapters are unwired, so every decision point runs its deterministic fallback and that path was exercised first.',
      'Automatic incident intake: Snapshots are captured on a schedule, reports are still operator-triggered.',
      'A fourth inbox category for model questions: the operator\'s diagram fixes three.',
      'A model-driven executor inside the Platform: the last gate before a machine changes stays deterministic.',
      'Develop worktrees, Bundle and WASM promotion, Team-internal parallel agents, SQLite.',
    ], 0.9, 2.45, 5.3, 4.3, { size: 11, after: 8 });
    card(s, 6.85, 1.5, 5.9, 5.35, { fill: C.ink, line: C.ink });
    await iconCircle(s, 'LuRoute', 7.15, 1.75, 0.55, C.moss, C.ink);
    text(s, 'Next slices, in order', 7.85, 1.78, 4.6, 0.5, { size: 15, bold: true, color: C.white, valign: 'middle' });
    const next = [
      ['1', 'Hybrid Snapshot Judge', 'deterministic rules always, an LLM over the Judge View; automatic Issue candidates'],
      ['2', 'Scheduler Policy model', 'behind advise_next_step, so today\'s chain rules become the harness around a model\'s judgment'],
      ['3', 'Human questions from a Team', 'NeedsHuman as an inbox interaction, once the operator rules on a fourth category'],
      ['4', 'Develop Teams', 'issue-scoped worktrees, tests, options, conflict detection'],
      ['5', 'Bundle and WASM promotion', 'with rollback, through the same ActionRun path'],
    ];
    let y = 2.45;
    for (const [num, hd, bd] of next) {
      s.addShape(pres.ShapeType.ellipse, { x: 7.15, y: y + 0.02, w: 0.38, h: 0.38, fill: { color: C.moss }, line: { color: C.moss, width: 0 } });
      text(s, num, 7.15, y + 0.02, 0.38, 0.38, { size: 11, bold: true, color: C.ink, align: 'center', valign: 'middle' });
      text(s, hd, 7.7, y, 4.8, 0.3, { size: 12, bold: true, color: C.white });
      text(s, bd, 7.7, y + 0.3, 4.8, 0.5, { size: 9.5, color: C.paleOnDark, lineSpacing: 1.05 });
      y += 0.85;
    }
    s.addNotes('Be explicit about what is not done and why. Two of the boundaries are rulings the operator made, not gaps. The next slices continue the same pattern: a model behind a port, validated by the harness.');
  }

  // 17. Closing --------------------------------------------------------------
  {
    const s = blank(C.ink); n++;
    text(s, 'The model proposes;', 0.8, 1.5, 11.5, 0.9, { font: F.head, size: 40, bold: true, color: C.white });
    text(s, 'the runtime records and enforces.', 0.8, 2.3, 11.5, 0.9, { font: F.head, size: 40, bold: true, color: C.moss });
    text(s, 'DEMO', 0.8, 3.65, 3, 0.3, { size: 10, bold: true, color: C.moss, charSpacing: 2 });
    const demo = [
      ['LuPlay', 'File a report; watch the Trace grow entry by entry; interrupt a second one.'],
      ['LuHand', 'Approve the held queue purge; reject one and send it back upstream; watch the revising pass read the feedback.'],
      ['LuCoins', 'Open Spend and Settings: a live value applies at once, a policy value waits for a freeze.'],
      ['LuFileJson', 'Export the session; import it elsewhere as a read-only archive.'],
      ['LuRotateCcw', 'Kill serve mid-pass and restart it: recovery, the frozen Scheduler, the interrupted Job in the inbox.'],
    ];
    let y = 4.05;
    for (const [ic, t] of demo) {
      await iconCircle(s, ic, 0.8, y, 0.42, C.forest, C.white);
      text(s, t, 1.4, y, 10.5, 0.42, { size: 12.5, color: C.white, valign: 'middle' });
      y += 0.56;
    }
    text(s, 'cargo run --example live_demo -- data-live   ·   cd web && pnpm dev   ·   docs/presentation/overview.md', 0.8, 6.95, 11, 0.3, { size: 9.5, color: C.paleOnDark, font: 'Courier New' });
    s.addNotes('Close on the sentence, then run the demo in this order. The live_demo example serves a scripted slow model on port 4720 so the demo needs neither a relay nor a deployment.');
  }

  // footers
  pres.slides.forEach((sl, i) => {
    if (i === 0) return;
    footer(sl, i + 1, i === pres.slides.length - 1);
  });

  await pres.writeFile({ fileName: OUT });
  console.log('wrote', OUT, 'slides:', pres.slides.length);
}

build().catch((e) => { console.error(e); process.exit(1); });
