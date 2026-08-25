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

// How much history to paint when a session opens. The server keeps about a
// week; this is roughly a screenful of scrollback.
const BACKLOG_LINES = 200;

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
  // The board is redrawn by two different polls: `/api/state` brings the
  // postings, `/api/me` brings this character's odds on them. Whichever
  // arrives second has to be able to redraw with what the first left here.
  quests: [],
  ascensionCr: Infinity,
};

// ---------------------------------------------------------------- requests

async function api(path, { method = 'GET', body, auth = false, token } = {}) {
  const headers = {};
  if (body !== undefined) headers['content-type'] = 'application/json';
  if (auth) {
    // An explicit token wins: the returning-player form has to prove a token
    // before anything is saved.
    const bearer = token ?? store.token;
    if (!bearer) throw new ApiError(401, 'You are not signed in.');
    headers.authorization = `Bearer ${bearer}`;
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

  // Stamp what the filter needs so a node can be re-judged later without
  // keeping the source line around. Nicks are `[A-Za-z0-9_-]`, so a space
  // join is unambiguous.
  li.dataset.kind = line.kind;
  li.dataset.actors = (line.actors ?? []).join(' ');
  li.hidden = filteredOut(li);

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
  updateFilterNote();
  if (atBottom) feed.scrollTop = feed.scrollHeight;
}

// ------------------------------------------------------------------ filter

// Purely a view: lines are hidden, never dropped, so relaxing the filter
// brings history straight back without refetching anything.
const filter = {
  hiddenKinds: new Set(),
  player: '',
  get active() { return this.hiddenKinds.size > 0 || this.player !== ''; },
};

function filteredOut(li) {
  if (filter.hiddenKinds.has(li.dataset.kind)) return true;
  if (!filter.player) return false;
  // Nicks are ASCII by the server's validator, so lowercasing is exact.
  return !(li.dataset.actors ?? '')
    .toLowerCase()
    .split(' ')
    .some((actor) => actor.includes(filter.player));
}

function applyFilter() {
  const feed = $('feed');
  for (const li of feed.children) li.hidden = filteredOut(li);
  $('filter-clear').hidden = !filter.active;
  updateFilterNote();
  // The visible tail just changed; land the reader on the latest of it.
  feed.scrollTop = feed.scrollHeight;
}

function updateFilterNote() {
  const feed = $('feed');
  const anyVisible = [...feed.children].some((li) => !li.hidden);
  $('filter-empty').hidden = !filter.active || anyVisible || !feed.childElementCount;
}

function renderBoard() {
  const board = $('board');
  const quests = state.quests;
  board.replaceChildren();
  $('board-count').textContent = `${quests.length} posted`;

  if (!quests.length) {
    board.append(emptyNote('The board is bare.'));
    return;
  }

  for (const quest of quests) {
    const li = document.createElement('li');
    const chance = outlookOf(quest.id);

    const id = document.createElement('span');
    id.className = 'qid';
    id.textContent = quest.id;

    const cr = document.createElement('span');
    cr.className = `qcr ${difficultyClass(quest.total_cr, chance)}`;
    cr.textContent = `CR ${quest.total_cr}`;

    const body = document.createElement('span');
    body.className = 'qbody';
    const name = document.createElement('span');
    name.className = 'qname';
    name.textContent = quest.display;
    body.append(name, questMeta(quest, chance));

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

/** What a posting pays, how it is likely to go, and whether it ends the game. */
function questMeta(quest, chance) {
  const meta = document.createElement('span');
  meta.className = 'qmeta';

  const purse = quest.reward.toLocaleString();
  const reward = document.createElement('span');
  reward.className = 'qreward';
  reward.textContent = `${purse} XP & coin`;
  reward.title = `Winning pays ${purse} experience and ${purse} coin. Losing pays nothing.`;
  meta.append(reward);

  if (chance !== undefined) {
    const band = outlookBand(chance);
    const odds = document.createElement('span');
    odds.className = `qodds ${band.className}`;
    odds.textContent = `${band.label} (${Math.round(chance * 100)}%)`;
    odds.title =
      `Estimated by fighting this warband ${state.me.outlook_trials} times over, `
      + 'with your current level and gear and the strategy you last fought with. '
      + 'A sample, not a promise.';
    meta.append(odds);
  }

  if (Number.parseFloat(quest.total_cr) >= state.ascensionCr) {
    const ascends = document.createElement('span');
    ascends.className = 'qascend';
    ascends.textContent = 'ascension';
    ascends.title =
      'At or above the ascension rating: winning this completes the game and '
      + 'your character leaves the Realm to start again.';
    meta.append(ascends);
  }

  return meta;
}

/** This character's sampled chance on a posting, or undefined if unjudged. */
function outlookOf(questId) {
  return state.me?.in_realm ? state.me.outlook?.[questId] : undefined;
}

/** How a sampled chance reads out loud. */
function outlookBand(chance) {
  if (chance >= 0.85) return { label: 'safe', className: 'odds-safe' };
  if (chance >= 0.6) return { label: 'favourable', className: 'odds-good' };
  if (chance >= 0.4) return { label: 'even', className: 'odds-even' };
  if (chance >= 0.15) return { label: 'risky', className: 'odds-risky' };
  return { label: 'grim', className: 'odds-grim' };
}

/**
 * Colour the challenge rating by what it means for the reader.
 *
 * The server's estimate is the honest answer, so it wins whenever there is one.
 * The level comparison is the fallback for a visitor with no character and for
 * a posting that went up since this character's sheet was last fetched.
 */
function difficultyClass(totalCr, chance) {
  if (chance !== undefined) {
    if (chance >= 0.6) return 'cr-easy';
    if (chance >= 0.25) return 'cr-fair';
    return 'cr-deadly';
  }
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
  $('player-names').replaceChildren(...scores.map((player) => {
    const option = document.createElement('option');
    option.value = player.nick;
    return option;
  }));
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

/** `1d6+2 (3-8)`: the weapon's die, the strength bonus, and one hit's range. */
function damageDisplay(damage) {
  const bonus = damage.bonus ?? 0;
  const sign = bonus > 0 ? `+${bonus}` : bonus < 0 ? String(bonus) : '';
  return `${damage.display}${sign} (${damage.min + bonus}-${damage.max + bonus})`;
}

/** Either the earned bonus to hit, or how many fights stand before it. */
function masteryDisplay(me) {
  if (me.proficiency_bonus) return `+${me.proficiency_bonus} to hit`;
  const mastery = me.mastery;
  return mastery ? `${mastery.fights} of ${mastery.required} fights` : '—';
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
  ];
  if (me.weapon?.damage) rows.push(['Damage', damageDisplay(me.weapon.damage)]);
  if (me.weapon) rows.push(['Mastery', masteryDisplay(me)]);
  rows.push(
    ['Armour', me.armor ? me.armor.name : '—'],
    ['Record', `${me.quests_won}W / ${me.quests_lost}L`],
  );

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
    // The board first: winning takes a posting off it and puts new ones up.
    // The sheet last, because its odds have to describe the board just drawn.
    await refreshState();
    await refreshMe();
  });
}

async function buyItem(itemId) {
  await withBusy(async () => {
    const turn = await api('/api/buy', { method: 'POST', auth: true, body: { item: itemId } });
    absorbTurn(turn);
    await pollFeed();
    // New gear moves the odds on every posting, so the sheet is refetched
    // after the board rather than before it.
    await refreshState();
    await refreshMe();
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

    state.quests = snapshot.quests;
    state.ascensionCr = Number.parseFloat(snapshot.ascension_cr);
    renderBoard();
    renderStore(snapshot.shop);
    renderScores(snapshot.scores);

    // The board rotates on its own, and nothing in the feed names us when it
    // does, so a rotation leaves the new postings unjudged until we ask again.
    if (
      !state.busy
      && state.me?.in_realm
      && state.quests.some((quest) => outlookOf(quest.id) === undefined)
    ) {
      refreshMe();
    }
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
    // Levelling up, buying a weapon or changing strategy all move the odds.
    renderBoard();
  } catch (error) {
    if (error.status === 401) handle(error);
  }
}

async function pollFeed() {
  try {
    // The first read pulls the recent backlog off the server, where history
    // survives reloads, sign-outs and new devices; after that the cursor
    // pages strictly forward.
    const query = state.cursor === 0
      ? `/api/feed?last=${BACKLOG_LINES}`
      : `/api/feed?since=${state.cursor}&limit=200`;
    const page = await api(query);
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
    // Show the token before anything else can happen to the page. If this
    // browser cannot keep it (a private window, blocked site data), the game
    // below would sign straight back out and reload; waiting here guarantees
    // the player saw their one credential first.
    await showFreshToken(result.nick, result.token);
    await enterRealm();
  } catch (failure) {
    error.textContent = failure.message;
    error.hidden = false;
  } finally {
    button.disabled = false;
  }
}

/** Present a just-minted token, resolving once the player dismisses it. */
function showFreshToken(nick, token) {
  return new Promise((resolve) => {
    $('welcome-nick').textContent = nick;
    $('welcome-token').value = token;
    const dialog = $('welcome-dialog');
    dialog.addEventListener('close', resolve, { once: true });
    dialog.showModal();
  });
}

/**
 * Come back with a saved token.
 *
 * The server keeps only the token's digest, so the token itself *is* the
 * credential: `/api/me` resolves it to a nick whether or not the character
 * still exists — a purged or ascended player signs back in and simply starts
 * their next character by questing.
 */
async function returnToRealm(event) {
  event.preventDefault();
  const token = $('return-token').value.trim();
  const button = $('return-submit');
  const error = $('return-error');

  button.disabled = true;
  error.hidden = true;
  try {
    const me = await api('/api/me', { auth: true, token });
    store.save(me.nick, token);
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

/** A click handler that copies one input's text and confirms on its button. */
function copyToken(inputId, buttonId) {
  return async () => {
    const input = $(inputId);
    try {
      await navigator.clipboard.writeText(input.value);
    } catch {
      // No clipboard permission — leave the text selected for a manual copy.
      input.select();
    }
    const button = $(buttonId);
    button.textContent = 'Copied';
    setTimeout(() => { button.textContent = 'Copy token'; }, 2_000);
  };
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
$('return-form').addEventListener('submit', returnToRealm);
$('strategy').addEventListener('change', () => {
  toast(`Strategy set to "${$('strategy').value}" for your next quest.`);
});

$('welcome-copy').addEventListener('click', copyToken('welcome-token', 'welcome-copy'));
$('welcome-continue').addEventListener('click', () => $('welcome-dialog').close());
// Do not leave the token sitting in the DOM once the dialog is dismissed.
$('welcome-dialog').addEventListener('close', () => { $('welcome-token').value = ''; });

$('sign-out').addEventListener('click', signOut);

for (const chip of document.querySelectorAll('#filter-kinds .chip')) {
  chip.addEventListener('click', () => {
    const kind = chip.dataset.kind;
    if (!filter.hiddenKinds.delete(kind)) filter.hiddenKinds.add(kind);
    chip.setAttribute('aria-pressed', String(!filter.hiddenKinds.has(kind)));
    applyFilter();
  });
}

$('filter-player').addEventListener('input', () => {
  filter.player = $('filter-player').value.trim().toLowerCase();
  applyFilter();
});

$('filter-clear').addEventListener('click', () => {
  filter.hiddenKinds.clear();
  filter.player = '';
  $('filter-player').value = '';
  for (const chip of document.querySelectorAll('#filter-kinds .chip')) {
    chip.setAttribute('aria-pressed', 'true');
  }
  applyFilter();
});

$('help-open').addEventListener('click', () => $('help-dialog').showModal());
$('help-close').addEventListener('click', () => $('help-dialog').close());

// A click on the backdrop lands on the <dialog> itself; treat it as a dismiss.
for (const dialog of document.querySelectorAll('dialog')) {
  dialog.addEventListener('click', (event) => {
    if (event.target === dialog) dialog.close();
  });
}

if (store.token) {
  enterRealm();
} else {
  // Still show the Realm's pulse to someone without an invitation.
  refreshState().then(() => { $('realm-status').hidden = false; });
}
