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

// The home-screen icon's badge: things that need you now (see /push/badge).
if ('setAppBadge' in navigator) {
  fetch('/push/badge', {cache: 'no-store'})
    .then((r) => r.ok ? r.json() : null)
    .then((j) => { if (j) return j.count ? navigator.setAppBadge(j.count) : navigator.clearAppBadge(); })
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
