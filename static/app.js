// baby-mcp pages: one small script; each part runs only on its own page.
'use strict';
async function post(url, body) {
  const r = await fetch(url, {method: 'POST', headers: {'content-type': 'application/json'}, body: JSON.stringify(body)});
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(j.error_description || j.error || ('HTTP ' + r.status));
  return j;
}
const say = (m) => document.getElementById('status').textContent = m;
const $ = (id) => document.getElementById(id);

// The home-screen icon's badge: things that need you now (see /push/badge);
// and close notifications that no longer apply (a cleared attention, a feed
// that happened).
if ('setAppBadge' in navigator || 'serviceWorker' in navigator) {
  fetch('/push/badge', {cache: 'no-store'})
    .then((r) => r.ok ? r.json() : null)
    .then(async (j) => {
      if (!j) return;
      if ('setAppBadge' in navigator) {
        try { await (j.count ? navigator.setAppBadge(j.count) : navigator.clearAppBadge()); } catch (e) { /* not allowed */ }
      }
      const reg = 'serviceWorker' in navigator && await navigator.serviceWorker.getRegistration();
      if (reg && reg.getNotifications && Array.isArray(j.groups)) {
        // Not one that just arrived (with the app open it isn't shown as a
        // banner, and the page reloads every minute).
        const fresh = (n) => n.data && n.data.at > Date.now() - 60 * 1000;
        for (const n of await reg.getNotifications()) {
          if (!fresh(n) && !j.groups.includes((n.data && n.data.group) || n.tag)) n.close();
        }
      }
    })
    .catch(() => {});
}

// Log in with a passkey.
if ($('login')) {
  $('login').onclick = async () => {
    try {
      say('Waiting for passkey…');
      const o = await post('/auth/login/begin', {next: $('login').dataset.next});
      const cred = await navigator.credentials.get({publicKey: PublicKeyCredential.parseRequestOptionsFromJSON(o.options.publicKey)});
      const r = await post('/auth/login/finish', {id: o.id, credential: cred.toJSON()});
      location.href = r.redirect;
    } catch (e) { say('Login failed: ' + e.message); }
  };
}

// Enroll a passkey.
if ($('enroll')) {
  $('enroll').onclick = async () => {
    try {
      const name = $('name').value.trim();
      if (!name) { say('Give it a name first.'); return; }
      say('Waiting for passkey…');
      const o = await post('/auth/enroll/begin', {token: $('enroll').dataset.token, name});
      const cred = await navigator.credentials.create({publicKey: PublicKeyCredential.parseCreationOptionsFromJSON(o.options.publicKey)});
      const r = await post('/auth/enroll/finish', {id: o.id, credential: cred.toJSON()});
      location.href = r.redirect;
    } catch (e) { say('Failed: ' + e.message); }
  };
}

// Notifications: this device, and the choices.
if ($('push')) {
  const KEY = document.getElementById('push').dataset.key;
  const dev = (m) => document.getElementById('dev').textContent = m;
  const show = (id, on) => document.getElementById(id).hidden = !on;
  function key(s) {
    s = s.replace(/-/g, '+').replace(/_/g, '/');
    s += '='.repeat((4 - s.length % 4) % 4);
    return Uint8Array.from(atob(s), c => c.charCodeAt(0));
  }
  async function refresh() {
    if (!('serviceWorker' in navigator) || !('PushManager' in window) || !('Notification' in window)) {
      dev('Notifications are not available here. On an iPhone: in Safari, Share → Add to Home Screen, then open the app from the home screen and come back to this page.');
      return;
    }
    const reg = await navigator.serviceWorker.register('/sw.js');
    await navigator.serviceWorker.ready;
    const sub = await reg.pushManager.getSubscription();
    if (sub && Notification.permission === 'granted') {
      await post('/push/subscribe', sub.toJSON()).catch(() => {});
      dev('On for this device.');
      show('on', false); show('off', true); show('test', true);
    } else if (Notification.permission === 'denied') {
      dev('Notifications are blocked for this site in the browser or phone settings.');
      show('on', false); show('off', false); show('test', false);
    } else {
      dev('Off for this device.');
      show('on', true); show('off', false); show('test', false);
    }
  }
  document.getElementById('on').onclick = async () => {
    try {
      const perm = await Notification.requestPermission();
      if (perm !== 'granted') { say('Not allowed.'); return refresh(); }
      const reg = await navigator.serviceWorker.ready;
      const sub = await reg.pushManager.subscribe({userVisibleOnly: true, applicationServerKey: key(KEY)});
      await post('/push/subscribe', sub.toJSON());
      say('Turned on.');
      location.reload();
    } catch (e) { say('Could not turn on: ' + e.message); }
  };
  document.getElementById('off').onclick = async () => {
    try {
      const reg = await navigator.serviceWorker.ready;
      const sub = await reg.pushManager.getSubscription();
      if (sub) { await post('/push/unsubscribe', {endpoint: sub.endpoint}); await sub.unsubscribe(); }
      location.reload();
    } catch (e) { say('Could not turn off: ' + e.message); }
  };
  document.getElementById('test').onclick = async () => {
    try {
      const r = await post('/push/test', {});
      say(r.sent ? 'Sent to ' + r.sent + ' device(s).' : 'No device accepted it; see the list below.');
    } catch (e) { say('Could not send: ' + e.message); }
  };
  for (const box of document.querySelectorAll('input[data-kind]')) {
    box.onchange = async () => {
      const note = document.getElementById('saved');
      try {
        await post('/push/prefs', {kind: box.dataset.kind, enabled: box.checked});
        note.textContent = 'Saved.';
      } catch (e) {
        box.checked = !box.checked;
        note.textContent = 'Could not save: ' + e.message;
      }
    };
  }
  refresh().catch(e => dev('Error: ' + e.message));
}

// Pull to refresh in the installed app (iOS gives home-screen apps none;
// a browser tab keeps its own).
(() => {
  const standalone = navigator.standalone === true || matchMedia('(display-mode: standalone)').matches;
  if (!standalone || !('ontouchstart' in window)) return;
  const el = document.createElement('div');
  el.id = 'ptr';
  el.textContent = '↓';
  document.body.prepend(el);
  const ready = 70;
  let start = null, dist = 0;
  const show = (d) => {
    el.style.transform = `translateY(${Math.min(d, 110) - 44}px)`;
    el.style.opacity = String(Math.min(1, d / ready));
    el.textContent = d >= ready ? '↻' : '↓';
  };
  addEventListener('touchstart', (e) => {
    start = window.scrollY <= 0 && e.touches.length === 1 ? e.touches[0].clientY : null;
    dist = 0;
  }, {passive: true});
  addEventListener('touchmove', (e) => {
    if (start === null) return;
    dist = e.touches[0].clientY - start;
    if (dist > 0) show(dist);
  }, {passive: true});
  addEventListener('touchend', () => {
    if (start !== null && dist >= ready) {
      el.classList.add('busy');
      location.reload();
    } else {
      el.style.transform = '';
      el.style.opacity = '0';
    }
    start = null;
    dist = 0;
  });
})();

// The timeline: one long strip from the day of birth to the hours ahead,
// scrolling sideways. Days load as they come into view (and far ones are
// dropped); the log below follows the day in view; pinch (or ctrl + wheel)
// changes the scale. Opens with "now" in view, on the day of `?day=`, or
// where it was before a recent reload.
(() => {
  const tl = $('tl');
  if (!tl) return;
  const d = tl.dataset, svg = tl.querySelector('svg.tl'), log = $('daylog');
  const SVGNS = 'http://www.w3.org/2000/svg';
  const PXH = d.pxh.split(',').map(Number), PAD = Number(d.pad);
  const FROM = Number(d.from), TO = Number(d.to), NOW = Number(d.now), TODAY = d.today;
  const TODAY_END = Number(d.todayEnd);
  const days = d.days.split(',').map((s) => { const [date, start] = s.split(':'); return {date, start: Number(start)}; });
  days.forEach((day, i) => { day.end = i + 1 < days.length ? days[i + 1].start : TO; });
  let z = Number(d.z);
  const x = (t) => PAD + (t - FROM) / 3600 * PXH[z];
  const tAt = (px) => FROM + (px - PAD) / PXH[z] * 3600;
  const W = () => tl.clientWidth;
  const loaded = new Map(); // date → its svg, or 'loading'
  for (const el of svg.querySelectorAll(':scope > svg[data-day]')) loaded.set(el.dataset.day, el);
  let gen = 0; // bumped on a zoom change: older loads are dropped

  // Days in date order, so a later one paints over an earlier one's edge.
  function place(el) {
    const after = [...svg.children].find((o) => o.dataset.day && o.dataset.day > el.dataset.day);
    svg.insertBefore(el, after || null);
  }
  async function fetchDay(day, zz) {
    const r = await fetch(`/timeline?date=${day.date}&z=${zz}`, {cache: 'no-store'});
    if (!r.ok) throw new Error('HTTP ' + r.status);
    const tmp = document.createElementNS(SVGNS, 'svg');
    tmp.innerHTML = await r.text();
    return tmp.firstElementChild;
  }
  // Load the days within a screen of the view; drop those beyond four.
  function fill() {
    const sl = tl.scrollLeft, w = W();
    const [a, b] = [tAt(sl - w), tAt(sl + 2 * w)], [fa, fb] = [tAt(sl - 4 * w), tAt(sl + 5 * w)];
    for (const day of days) {
      const have = loaded.get(day.date);
      if (day.end > a && day.start < b) {
        if (have) continue;
        loaded.set(day.date, 'loading');
        const g = gen;
        fetchDay(day, z).then((el) => {
          if (g !== gen) return;
          loaded.set(day.date, el);
          place(el);
        }).catch(() => { if (g === gen) loaded.delete(day.date); });
      } else if (have instanceof Element && (day.end < fa || day.start > fb)) {
        have.remove();
        loaded.delete(day.date);
      }
    }
  }

  // The day in view: today while "now" is on screen, else the one in the middle.
  const dayAt = (t) => days.reduce((r, day) => (day.start <= t ? day : r), days[0]);
  function inView() {
    const n = x(NOW);
    if (n >= tl.scrollLeft && n <= tl.scrollLeft + W()) return TODAY;
    return dayAt(tAt(tl.scrollLeft + W() / 2)).date;
  }
  // Day logs fetched, kept briefly (the page reloads every minute anyway).
  const logs = new Map();
  const getLog = (date) => {
    if (!logs.has(date)) {
      logs.set(date, fetch('/log?date=' + date, {cache: 'no-store'})
        .then((r) => { if (!r.ok) throw new Error('HTTP ' + r.status); return r.text(); })
        .catch((e) => { logs.delete(date); throw e; }));
    }
    return logs.get(date);
  };
  const near = (date, k) => { const i = days.findIndex((dd) => dd.date === date); return days[i + k] && days[i + k].date; };
  // A bar under the day the log shows.
  const bar = document.createElementNS(SVGNS, 'rect');
  bar.setAttribute('class', 'on');
  bar.setAttribute('height', '3');
  bar.setAttribute('rx', '1.5');
  bar.setAttribute('y', String(Number(svg.getAttribute('height')) - 4));
  // With what's next (today), the hours ahead past midnight: dashed.
  const ahead = document.createElementNS(SVGNS, 'line');
  ahead.setAttribute('class', 'on-next');
  ahead.setAttribute('y1', bar.getAttribute('y') * 1 + 1.5);
  ahead.setAttribute('y2', bar.getAttribute('y') * 1 + 1.5);
  function mark(date) {
    const day = days.find((dd) => dd.date === date);
    if (!day) return;
    const a = x(day.start) + 2, b = x(date === TODAY ? Math.min(day.end, TODAY_END) : day.end) - 2;
    bar.setAttribute('x', String(a));
    bar.setAttribute('width', String(Math.max(0, b - a)));
    svg.appendChild(bar);
    if (date === TODAY && TO > TODAY_END) {
      ahead.setAttribute('x1', String(x(TODAY_END) + 3.5));
      ahead.setAttribute('x2', String(x(TO) - 1.5));
      svg.appendChild(ahead);
    } else {
      ahead.remove();
    }
  }
  let shown = log ? log.dataset.day : TODAY, seq = 0;
  mark(shown);
  async function show(date) {
    if (!log || date === shown) return;
    const was = shown, my = ++seq;
    shown = date;
    mark(date);
    try {
      const html = await getLog(date);
      if (my !== seq) return;
      log.innerHTML = html;
      log.dataset.day = date;
      for (const el of document.querySelectorAll('.now-only')) el.hidden = date !== TODAY;
      // The days next to it, for scrolling on.
      for (const k of [-1, 1]) { const n = near(date, k); if (n) getLog(n).catch(() => {}); }
    } catch (e) {
      if (my === seq) { shown = was; mark(was); }
    }
  }
  // The address follows when scrolling stops (Safari limits how often).
  function setUrl() {
    const url = shown === TODAY ? '/' : '/?day=' + shown;
    if (location.pathname + location.search !== url) try { history.replaceState(null, '', url); } catch (e) { /* too often */ }
  }
  const home = () => x(NOW) - W() * Number(d.at);
  function goTo(date, smooth) {
    const day = days.find((dd) => dd.date === date);
    if (!day) return;
    const left = date === TODAY ? home() : x((day.start + day.end) / 2) - W() / 2;
    tl.scrollTo({left, behavior: smooth ? 'smooth' : 'auto'});
    show(date).then(setUrl);
  }
  // The links to other days in the log scroll the timeline there.
  if (log) {
    log.addEventListener('click', (e) => {
      const a = e.target.closest('a[data-day]');
      if (!a || e.metaKey || e.ctrlKey || e.shiftKey) return;
      e.preventDefault();
      goTo(a.dataset.day, true);
    });
  }

  // Where to open.
  let saved = null;
  try {
    const s = JSON.parse(sessionStorage.getItem('tl2') || 'null');
    if (s && Date.now() - s.t < 3 * 60 * 1000 && s.url === location.pathname + location.search) saved = s;
  } catch (e) { /* no storage */ }
  if (saved) tl.scrollLeft = x(saved.c) - W() / 2;
  else if (d.focus) tl.scrollLeft = x(Number(d.focus)) - W() / 2;
  else tl.scrollLeft = home();
  const save = () => {
    try { sessionStorage.setItem('tl2', JSON.stringify({t: Date.now(), c: tAt(tl.scrollLeft + W() / 2), url: location.pathname + location.search})); } catch (e) { /* no storage */ }
  };
  let touched = false;
  for (const ev of ['touchstart', 'pointerdown', 'wheel']) tl.addEventListener(ev, () => { touched = true; }, {passive: true});
  addEventListener('pagehide', () => { if (touched) save(); });
  let busy = false, idle;
  tl.addEventListener('scroll', () => {
    if (!busy) {
      busy = true;
      requestAnimationFrame(() => {
        busy = false;
        fill();
        if (touched) show(inView());
      });
    }
    clearTimeout(idle);
    idle = setTimeout(() => {
      if (!touched) return;
      save();
      setUrl();
    }, 250);
  }, {passive: true});
  fill();

  // Zoom: while pinching, the strip is stretched; on letting go, the
  // nearest level is loaded, keeping the time under the fingers in place.
  const clamp = (s) => Math.min(PXH[0] * 1.5 / PXH[z], Math.max(PXH[PXH.length - 1] * 0.6 / PXH[z], s));
  function preview(p) {
    svg.style.transformOrigin = `${p.ax}px 0`;
    svg.style.transform = `scaleX(${p.s})`;
  }
  const unstretch = () => { svg.style.transform = ''; };
  function level(s) {
    const want = PXH[z] * s;
    let best = z;
    PXH.forEach((p, i) => { if (Math.abs(Math.log(p / want)) < Math.abs(Math.log(PXH[best] / want))) best = i; });
    if (best === z && s > 1.25 && z > 0) best = z - 1;
    if (best === z && s < 0.8 && z < PXH.length - 1) best = z + 1;
    return best;
  }
  async function commit(p) {
    const nz = level(p.s);
    if (nz === z) { unstretch(); return; }
    const g = ++gen;
    z = nz;
    try { document.cookie = `tlz=${z}; path=/; max-age=31536000; samesite=lax`; } catch (e) { /* none */ }
    const left = x(p.t) - p.sx, w = W();
    const [a, b] = [tAt(left - w), tAt(left + 2 * w)];
    const need = days.filter((day) => day.end > a && day.start < b);
    const els = await Promise.all(need.map((day) => fetchDay(day, z).catch(() => null)));
    if (g !== gen) return;
    loaded.clear();
    for (const el of [...svg.querySelectorAll(':scope > svg')]) el.remove();
    svg.setAttribute('width', String(Math.round(x(TO) + PAD)));
    unstretch();
    els.forEach((el, i) => { if (el) { loaded.set(need[i].date, el); place(el); } });
    mark(shown);
    tl.scrollLeft = left;
    fill();
    save();
    show(inView()).then(setUrl);
  }
  const start = (sx) => ({sx, ax: tl.scrollLeft + sx, t: tAt(tl.scrollLeft + sx), s: 1});
  const left0 = () => tl.getBoundingClientRect().left;
  let pinch = null;
  const span = (e) => Math.hypot(e.touches[0].clientX - e.touches[1].clientX, e.touches[0].clientY - e.touches[1].clientY);
  tl.addEventListener('touchstart', (e) => {
    if (e.touches.length !== 2) return;
    pinch = start((e.touches[0].clientX + e.touches[1].clientX) / 2 - left0());
    pinch.d = span(e);
  }, {passive: true});
  tl.addEventListener('touchmove', (e) => {
    if (!pinch || e.touches.length !== 2) return;
    e.preventDefault();
    pinch.s = clamp(span(e) / pinch.d);
    preview(pinch);
  }, {passive: false});
  const end = () => { if (pinch) { const p = pinch; pinch = null; commit(p); } };
  tl.addEventListener('touchend', end);
  tl.addEventListener('touchcancel', end);
  // Safari: no page zoom on the strip; on a Mac, its trackpad pinch.
  const touch = 'ontouchstart' in window;
  let gest = null;
  tl.addEventListener('gesturestart', (e) => { e.preventDefault(); if (!touch) gest = start(e.clientX - left0()); });
  tl.addEventListener('gesturechange', (e) => { e.preventDefault(); if (gest) { gest.s = clamp(e.scale); preview(gest); } });
  tl.addEventListener('gestureend', (e) => { e.preventDefault(); if (gest) { const p = gest; gest = null; commit(p); } });
  // Other browsers: a trackpad pinch is a wheel with ctrl.
  let wheel = null, wt;
  tl.addEventListener('wheel', (e) => {
    if (!e.ctrlKey) return;
    e.preventDefault();
    if (!wheel) wheel = start(e.clientX - left0());
    wheel.s = clamp(wheel.s * Math.exp(-e.deltaY / 100));
    preview(wheel);
    clearTimeout(wt);
    wt = setTimeout(() => { const p = wheel; wheel = null; commit(p); }, 250);
  }, {passive: false});
})();

// Charts: view as a user or as a viewer (a parent's preview).
if ($('viewas')) {
  $('viewas').onchange = (e) => { location.href = e.target.value === 'viewer' ? '/charts?as=viewer' : '/charts'; };
}
