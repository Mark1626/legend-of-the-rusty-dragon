// Legend of the Rusty Dragon — client.
//
// No framework and no build step: the whole thing is one fetch loop over an
// API that already returns rendered lines.
//
// One rule throughout: build DOM nodes, never HTML strings. The API guarantees
// that user-controlled text is `[A-Za-z0-9_-]`, but that guarantee lives in one
// server-side validator, and the day it relaxes to allow Unicode display names
// a string-concatenating client would go from safe to stored-XSS across every
// page at once. `textContent` is immune either way.

'use strict';

const TOKEN_KEY = 'rusty-dragon.token';
const NICK_KEY = 'rusty-dragon.nick';

// `/api/feed` never touches the game lock, so it is cheap to poll often.
// `/api/state` can advance the world's clock, so it goes slower.
const FEED_INTERVAL_MS = 4_000;
const STATE_INTERVAL_MS = 20_000;

const $ = (id) => document.getElementById(id);

const store = {
  get token() { return safeRead(TOKEN_KEY); },
  get nick() { return safeRead(NICK_KEY); },
  save(nick, token) { safeWrite(NICK_KEY, nick); safeWrite(TOKEN_KEY, token); },
  clear() { safeWrite(NICK_KEY, null); safeWrite(TOKEN_KEY, null); },
};

// Private windows and blocked site data make these throw rather than return
// null, so every access is guarded.
function safeRead(key) {
  try { return localStorage.getItem(key); } catch { return null; }
}
function safeWrite(key, value) {
  try { value === null ? localStorage.removeItem(key) : localStorage.setItem(key, value); }
  catch { /* the session simply will not be remembered */ }
}

const state = {
  cursor: 0,
  me: null,
  restUntil: 0,   // client clock, milliseconds
  serverSkewMs: 0,
  seenQuests: new Set(),
  busy: false,
};

// ---------------------------------------------------------------- requests

async function api(path, { method = 'GET', body, auth = false } = {}) {
  const headers = {};
  if (body !== undefined) headers['content-type'] = 'application/json';
  if (auth) {
    const token = store.token;
    if (!token) throw new ApiError(401, 'You are not signed in.');
    headers.authorization = `Bearer ${token}`;
  }

  let response;
  try {
    response = await fetch(path, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    throw new ApiError(0, 'Could not reach the Realm.');
  }

  const payload = await response.json().catch(() => null);
  if (!response.ok) {
    throw new ApiError(response.status, payload?.error ?? `Request failed (${response.status}).`);
  }
  return payload;
}

class ApiError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}

// ------------------------------------------------------------------ render

/** Turn one API line into a list item. */
function lineNode(line) {
  const li = document.createElement('li');

  const stamp = document.createElement('span');
  stamp.className = 'stamp';
  stamp.textContent = clockOf(line.at);
  li.append(stamp);

  const badge = document.createElement('span');
  badge.className = `badge ${line.kind}`;
  badge.textContent = line.kind === 'b_board' ? 'BOARD' : line.kind.toUpperCase();
  li.append(badge);

  const body = document.createElement('span');
  for (const span of line.spans ?? []) {
    const el = document.createElement('span');
    el.textContent = span.text;              // never innerHTML
    if (span.bold) el.classList.add('b');
    if (span.color && span.color !== 'default') el.classList.add(`c-${span.color}`);
    body.append(el);
  }
  if (!body.childNodes.length) body.textContent = line.text ?? '';
  li.append(body);

  return li;
}

function appendFeed(lines) {
  if (!lines.length) return;
  const feed = $('feed');
  // Only stick to the bottom if the reader is already there — scrolling up to
  // read history should not be yanked away.
  const atBottom = feed.scrollHeight - feed.scrollTop - feed.clientHeight < 60;

  for (const line of lines) {
    const node = lineNode(line);
    node.classList.add('fresh');
    feed.append(node);
  }
  while (feed.childElementCount > 400) feed.firstElementChild.remove();
  if (atBottom) feed.scrollTop = feed.scrollHeight;
}

function renderBoard(quests) {
  const board = $('board');
  board.replaceChildren();
  $('board-count').textContent = `${quests.length} posted`;

  if (!quests.length) {
    board.append(emptyNote('The board is bare.'));
    return;
  }

  for (const quest of quests) {
    const li = document.createElement('li');

    const id = document.createElement('span');
    id.className = 'qid';
    id.textContent = quest.id;

    const cr = document.createElement('span');
    cr.className = `qcr ${difficultyClass(quest.total_cr)}`;
    cr.textContent = `CR ${quest.total_cr}`;

    const body = document.createElement('span');
    body.className = 'qbody';
    const name = document.createElement('span');
    name.className = 'qname';
    name.textContent = quest.display;
    body.append(name);

    const go = document.createElement('button');
    go.className = 'act';
    go.type = 'button';
    go.textContent = 'Accept';
    go.disabled = state.busy || resting();
    go.addEventListener('click', () => acceptQuest(quest.id));

    li.append(id, cr, body, go);
    board.append(li);
  }
}

/** A rough cue for whether a quest is worth trying, given the player's level. */
function difficultyClass(totalCr) {
  const cr = Number.parseFloat(totalCr);
  const level = state.me?.level ?? 1;
  if (!Number.isFinite(cr)) return '';
  if (cr <= level) return 'cr-easy';
  if (cr <= level * 2) return 'cr-fair';
  return 'cr-deadly';
}

function renderStore(items) {
  const list = $('store');
  list.replaceChildren();
  if (!items.length) {
    list.append(emptyNote('The shelves are empty.'));
    return;
  }

  const purse = state.me?.money ?? 0;
  for (const item of items) {
    const li = document.createElement('li');

    const tag = document.createElement('span');
    tag.className = 'iid';
    tag.textContent = item.tag;

    const body = document.createElement('span');
    body.className = 'ibody';
    const name = document.createElement('span');
    name.textContent = item.name;
    const detail = document.createElement('span');
    detail.className = 'detail';
    detail.textContent = ` ${describeItem(item)}`;
    body.append(name, detail);

    const cost = document.createElement('span');
    cost.className = 'cost';
    cost.textContent = item.cost.toLocaleString();

    const buy = document.createElement('button');
    buy.className = 'act';
    buy.type = 'button';
    buy.textContent = 'Buy';
    buy.disabled = state.busy || purse < item.cost;
    buy.title = purse < item.cost ? 'You cannot afford this yet.' : `Buy a ${item.name}`;
    buy.addEventListener('click', () => buyItem(item.id));

    li.append(tag, body, cost, buy);
    list.append(li);
  }
}

function describeItem(item) {
  const d = item.detail ?? {};
  if (item.kind === 'weapon') return d.damage ? `· ${d.damage}` : '';
  const bonus = d.dex_bonus
    ? (d.max_dex_bonus === null || d.max_dex_bonus === undefined ? ' +dex' : ` +dex (max ${d.max_dex_bonus})`)
    : '';
  return `· AC ${d.armor_class}${bonus}`;
}

function renderScores(scores) {
  const list = $('scores');
  list.replaceChildren();
  if (!scores.length) {
    list.append(emptyNote('Nobody has arrived yet.'));
    return;
  }
  scores.forEach((player, index) => {
    const li = document.createElement('li');

    const rank = document.createElement('span');
    rank.className = 'rank';
    rank.textContent = `${index + 1}.`;

    const who = document.createElement('span');
    who.className = 'who';
    if (player.nick === store.nick) who.classList.add('self');
    who.textContent = player.nick;
    if (player.resting) who.classList.add('zzz');

    const lvl = document.createElement('span');
    lvl.className = 'lvl';
    lvl.textContent = `lvl ${player.level} · ${player.xp.toLocaleString()} xp`;

    li.append(rank, who, lvl);
    list.append(li);
  });
}

function renderSheet(me) {
  const sheet = $('sheet');
  sheet.replaceChildren();
  const absent = !me?.in_realm;
  $('sheet-absent').hidden = !absent;
  sheet.hidden = absent;
  if (absent) { $('rest').hidden = true; return; }

  const rows = [
    ['Level', me.level],
    ['Experience', me.xp.toLocaleString()],
    ['Coin', me.money.toLocaleString()],
    ['Hit points', me.hp],
    ['Armour class', me.ac],
    ['Strength', me.strength],
    ['Dexterity', me.dexterity],
    ['Weapon', me.weapon ? me.weapon.name : '—'],
    ['Armour', me.armor ? me.armor.name : '—'],
    ['Record', `${me.quests_won}W / ${me.quests_lost}L`],
  ];
  if (me.proficiency_bonus) rows.splice(7, 0, ['Proficiency', `+${me.proficiency_bonus}`]);

  for (const [label, value] of rows) {
    const dt = document.createElement('dt');
    dt.textContent = label;
    const dd = document.createElement('dd');
    dd.textContent = String(value);
    sheet.append(dt, dd);
  }

  if (me.strategy) $('strategy').value = me.strategy;
}

function emptyNote(text) {
  const li = document.createElement('li');
  li.className = 'empty';
  li.textContent = text;
  return li;
}

// -------------------------------------------------------------------- time

/** The server's clock, expressed in the browser's. */
function serverNow() { return Date.now() + state.serverSkewMs; }

function clockOf(unixSeconds) {
  if (!unixSeconds) return '--:--';
  return new Date(unixSeconds * 1000)
    .toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hour12: false });
}

function countdown(seconds) {
  const s = Math.max(0, Math.ceil(seconds));
  const m = Math.floor(s / 60);
  return m > 0 ? `${m}m ${String(s % 60).padStart(2, '0')}s` : `${s}s`;
}

function resting() { return state.restUntil > serverNow(); }

/** Ticks once a second so the rest timer runs down without polling. */
function startRestClock() {
  setInterval(() => {
    const remaining = (state.restUntil - serverNow()) / 1000;
    const rest = $('rest');
    if (remaining > 0) {
      rest.hidden = false;
      $('rest-clock').textContent = countdown(remaining);
    } else if (!rest.hidden) {
      rest.hidden = true;
      // Rest just ended — let the board offer its quests again.
      refreshState();
    }
    for (const button of document.querySelectorAll('#board .act')) {
      button.disabled = state.busy || remaining > 0;
    }
  }, 1000);
}

// ------------------------------------------------------------------ actions

async function acceptQuest(questId) {
  const strategy = $('strategy').value;
  await withBusy(async () => {
    const turn = await api('/api/quest', {
      method: 'POST', auth: true,
      body: { quest: questId, strategy },
    });
    absorbTurn(turn);
    if (turn.ascension) toast(turn.ascension, false, 15_000);
    await pollFeed();          // our own narration, without waiting for the tick
    await refreshMe();
    await refreshState();
  });
}

async function buyItem(itemId) {
  await withBusy(async () => {
    const turn = await api('/api/buy', { method: 'POST', auth: true, body: { item: itemId } });
    absorbTurn(turn);
    await pollFeed();
    await refreshMe();
    await refreshState();
  });
}

/**
 * Show a turn's private reply.
 *
 * Deliberately does *not* render `turn.feed` or advance the cursor from
 * `turn.cursor`. Feed ids are global, so another player's line can land
 * between ours: jumping the cursor straight to our own last id would step over
 * theirs and lose it for good. The caller polls immediately instead, which
 * costs one cheap request and keeps the channel gapless.
 */
function absorbTurn(turn) {
  const reply = (turn.reply ?? []).map((line) => line.text).filter(Boolean).join(' ');
  if (reply) toast(reply);
}

async function withBusy(work) {
  if (state.busy) return;
  state.busy = true;
  try {
    await work();
  } catch (error) {
    handle(error);
  } finally {
    state.busy = false;
  }
}

function handle(error) {
  if (error.status === 401) {
    toast('Your session is no longer valid. Signing out.', true);
    signOut();
    return;
  }
  toast(error.message ?? 'Something went wrong.', true);
}

let toastTimer;
function toast(message, bad = false, ms = 6_000) {
  const el = $('toast');
  el.textContent = message;
  el.classList.toggle('bad', bad);
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { el.hidden = true; }, ms);
}

// ------------------------------------------------------------------- polls

async function refreshState() {
  try {
    const snapshot = await api('/api/state');
    state.serverSkewMs = snapshot.now * 1000 - Date.now();

    $('realm-status').hidden = false;
    $('realm-players').textContent = snapshot.players;
    $('realm-uptime').textContent = snapshot.uptime_display;
    $('realm-ascension').textContent = snapshot.ascension_cr;

    renderBoard(snapshot.quests);
    renderStore(snapshot.shop);
    renderScores(snapshot.scores);

    // A brand-new session starts at the tip rather than replaying a week.
    if (state.cursor === 0) state.cursor = snapshot.cursor;
  } catch (error) {
    if (error.status === 0) $('feed-status').textContent = 'offline';
  }
}

async function refreshMe() {
  try {
    const me = await api('/api/me', { auth: true });
    state.me = me;
    state.restUntil = me.in_realm ? serverNow() + me.resting_for * 1000 : 0;
    renderSheet(me);
  } catch (error) {
    if (error.status === 401) handle(error);
  }
}

async function pollFeed() {
  try {
    const page = await api(`/api/feed?since=${state.cursor}&limit=200`);
    if (page.lines.length) {
      appendFeed(page.lines);
      state.cursor = page.cursor;
      // Anything naming us may have changed our sheet — XP, coin, a theft.
      if (page.lines.some((line) => (line.actors ?? []).includes(store.nick))) {
        refreshMe();
      }
    }
    $('feed-status').textContent = '';
  } catch (error) {
    $('feed-status').textContent = error.status === 0 ? 'offline' : '';
  }
}

// -------------------------------------------------------------------- gate

async function join(event) {
  event.preventDefault();
  const nick = $('join-nick').value.trim();
  const invite = $('join-invite').value;
  const button = $('join-submit');
  const error = $('join-error');

  button.disabled = true;
  error.hidden = true;
  try {
    const result = await api('/api/join', { method: 'POST', body: { nick, invite } });
    store.save(result.nick, result.token);
    await enterRealm();
  } catch (failure) {
    error.textContent = failure.message;
    error.hidden = false;
  } finally {
    button.disabled = false;
  }
}

function signOut() {
  store.clear();
  location.reload();
}

async function enterRealm() {
  $('gate').hidden = true;
  $('game').hidden = false;
  $('whoami').hidden = false;
  $('whoami-nick').textContent = store.nick ?? '';

  await refreshState();
  await refreshMe();
  await pollFeed();

  setInterval(pollFeed, FEED_INTERVAL_MS);
  setInterval(refreshState, STATE_INTERVAL_MS);
  startRestClock();
}

// -------------------------------------------------------------------- boot

$('join-form').addEventListener('submit', join);
$('sign-out').addEventListener('click', signOut);
$('strategy').addEventListener('change', () => {
  toast(`Strategy set to "${$('strategy').value}" for your next quest.`);
});

if (store.token) {
  enterRealm();
} else {
  // Still show the Realm's pulse to someone without an invitation.
  refreshState().then(() => { $('realm-status').hidden = false; });
}
