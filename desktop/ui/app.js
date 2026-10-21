const invoke = (command, args = {}) => {
  if (!window.__TAURI__) return Promise.reject(new Error('Open the desktop app.'));
  return window.__TAURI__.core.invoke(command, args);
};
const $ = id => document.getElementById(id);
let settings, addresses = [], profiles = [], running = false, busy = false, pendingRole, cursor = 0, logs = [], saveQueue = Promise.resolve();
const optionIds = ['connect','listen','key_file','log_file','verbose','discovery','once','snapshot_mode','reconcile_ms','verify_secs','tls','tls_cert','tls_key','tls_ca','tls_server_name','journal_chunk_kib'];
const options = () => settings[settings.role];
const showError = error => { $('error').textContent = String(error); $('error').hidden = false; };
const clearError = () => { $('error').hidden = true; };
const bytes = value => value < 1024 ? `${value} B` : value < 1048576 ? `${(value / 1024).toFixed(1)} KiB` : `${(value / 1048576).toFixed(1)} MiB`;

function save() {
  const value = structuredClone(settings);
  $('save-state').textContent = 'Saving…';
  saveQueue = saveQueue.catch(() => {}).then(() => invoke('save_settings', { settings: value })).then(() => {
    $('save-state').textContent = 'Saved';
  }).catch(error => { $('save-state').textContent = 'Save failed'; showError(error); });
  return saveQueue;
}
function readForm() {
  for (const id of optionIds) {
    const input = $(id);
    options()[id] = input.type === 'checkbox' ? input.checked : input.type === 'number' ? Number(input.value) : input.value.trim();
  }
  options()[settings.role === 'send' ? 'source' : 'destination'] = $('directory').value.trim();
}
function renderProfiles() {
  const select = $('profiles');
  select.replaceChildren(new Option('Choose a profile…', ''));
  profiles.forEach((profile, index) => {
    if (settings.role === 'send' && !profile.has_journals) return;
    const label = `${profile.profile} · ${profile.prefix}`;
    const entry = new Option(label, String(index));
    entry.title = label;
    entry.selected = profile.destination === $('directory').value;
    select.add(entry);
  });
}
function render() {
  const send = settings.role === 'send';
  $('heading').textContent = send ? 'Send journals' : 'Receive journals';
  $('description').textContent = send ? 'Send live journal updates.' : 'Write journals for companion apps.';
  $('directory-heading').textContent = send ? 'Source' : 'Destination';
  $('directory-help').textContent = send ? 'Sends Journal.*.log and JSON snapshots. Source files stay read-only.' : 'Files are written here. Deleted source files are kept.';
  $('prefix-help').textContent = send ? 'Only profiles with journals are listed. You can also browse.' : 'Choose the prefix used by your companion apps.';
  $('remote-section').hidden = send;
  $('server-addresses').hidden = !send || !addresses.length;
  $('server-addresses').textContent = 'Addresses: ' + addresses.join(', ');
  $('send-options').hidden = !send;
  $('receive-options').hidden = send;
  $('send-role').setAttribute('aria-pressed', String(send));
  $('receive-role').setAttribute('aria-pressed', String(!send));
  $('directory').value = options()[send ? 'source' : 'destination'];
  for (const id of optionIds) {
    if ($(id).type === 'checkbox') $(id).checked = options()[id];
    else $(id).value = options()[id];
  }
  $('remotes').replaceChildren(...settings.remotes.map(remote => new Option(remote, remote)));
  renderProfiles(); renderSecurity(); lock();
}
function renderSecurity() {
  const tls = options().tls;
  $('tls-options').hidden = !tls;
  $('tls-send').hidden = settings.role !== 'send';
  $('tls-receive').hidden = settings.role !== 'receive';
  $('auto').disabled = tls;
  $('discovery').disabled = tls;
  $('transport-help').textContent = tls ? 'Set an address and certificate under Advanced.' : 'Plain TCP is for trusted networks or an encrypted tunnel.';
  $('security-summary').textContent = tls ? 'TLS encrypted · Shared-key authenticated' : 'TCP authenticated · Not encrypted';
  if (tls) $('advanced').open = true;
}
function lock() {
  $('configuration').disabled = busy || running;
  $('start').disabled = busy || running || !settings;
  $('start').textContent = busy ? 'Working…' : settings?.role === 'send' ? 'Start sending' : 'Start receiving';
  $('stop').disabled = busy || !running;
  $('send-role').disabled = busy;
  $('receive-role').disabled = busy;
  $('confirm-switch').disabled = busy;
}
async function perform(action) {
  if (busy) return;
  busy = true; clearError(); lock();
  try { await action(); } catch (error) { showError(error); }
  finally { busy = false; lock(); }
}
async function changeRole(role) {
  if (role === settings.role) return;
  if (running) {
    pendingRole = role;
    $('switch-message').textContent = `Stop and switch to ${role === 'send' ? 'Send' : 'Receive'}?`;
    $('switch-notice').hidden = false;
    return;
  }
  settings.role = role; render(); await save();
}
$('send-role').onclick = () => changeRole('send');
$('receive-role').onclick = () => changeRole('receive');
$('cancel-switch').onclick = () => { pendingRole = undefined; $('switch-notice').hidden = true; };
$('confirm-switch').onclick = () => perform(async () => {
  await invoke('stop_relay'); running = false;
  settings.role = pendingRole; pendingRole = undefined;
  $('switch-notice').hidden = true; render(); await save(); await refresh();
});
$('relay-form').onchange = event => { if (!running && event.target.id !== 'profiles') { readForm(); renderSecurity(); save(); } };
$('relay-form').onsubmit = event => {
  event.preventDefault();
  perform(async () => {
    readForm();
    if (settings.role === 'receive' && options().connect !== 'auto' && !settings.remotes.includes(options().connect)) settings.remotes = [...settings.remotes.slice(-19), options().connect];
    await saveQueue; await invoke('start_relay', { settings });
    await refresh();
  });
};
$('stop').onclick = () => perform(async () => { await invoke('stop_relay'); await refresh(); });
$('auto').onclick = () => { $('connect').value = 'auto'; readForm(); save(); };
$('profiles').onchange = event => {
  const profile = profiles[Number(event.target.value)];
  if (event.target.value === '' || !profile) return;
  $('directory').value = profile.destination; readForm(); save();
};
async function browse(id, kind) {
  const path = await invoke('pick_path', { kind });
  if (path) { $(id).value = path; readForm(); await save(); }
}
$('browse-directory').onclick = () => perform(() => browse('directory','folder'));
$('browse-key').onclick = () => perform(() => browse('key_file','key'));
$('browse-cert').onclick = () => perform(() => browse('tls_cert','certificate'));
$('browse-private-key').onclick = () => perform(() => browse('tls_key','private-key'));
$('browse-ca').onclick = () => perform(() => browse('tls_ca','certificate'));
$('generate-tls').onclick = () => perform(async () => {
  const files = await invoke('create_tls', { name:$('tls_server_name').value.trim() });
  if (files) { $('tls_cert').value=files.certificate; $('tls_key').value=files.key; readForm(); await save(); }
});
$('browse-log').onclick = () => perform(() => browse('log_file','log'));
$('generate-key').onclick = () => perform(async () => {
  const path = await invoke('create_key');
  if (path) { $('key_file').value = path; readForm(); await save(); }
});
$('rescan').onclick = () => perform(async () => { profiles = await invoke('scan_prefixes'); renderProfiles(); });
$('choose-prefix').onclick = () => perform(async () => {
  const path = await invoke('pick_path', { kind:'prefix' });
  if (!path) return;
  const found = await invoke('inspect_prefix', { path });
  if (!found.length) throw new Error('No Windows users found. Run Wine in this prefix first.');
  for (const profile of found) if (!profiles.some(p => p.prefix === profile.prefix && p.profile === profile.profile)) profiles.push(profile);
  if (found.length === 1) { $('directory').value = found[0].destination; readForm(); await save(); }
  else { $('prefix-help').textContent = 'Choose the Windows user your companion uses.'; }
  renderProfiles();
});
function renderLogs() {
  const warnings = $('log-filter').value === 'warnings';
  const shown = logs.filter(log => !warnings || ['WARN','ERROR'].includes(log.level));
  const container = $('log'); const atEnd = container.scrollTop + container.clientHeight >= container.scrollHeight - 35;
  if (!shown.length) { const p = document.createElement('p'); p.className='log-empty'; p.textContent=warnings ? 'No warnings or errors.' : 'No activity yet.'; container.replaceChildren(p); return; }
  container.replaceChildren(...shown.map(log => {
    const line = document.createElement('div'); line.className = 'log-entry'; line.dataset.level = log.level;
    const time = document.createElement('time'); time.textContent = new Date(log.time).toLocaleTimeString();
    const detail = Object.entries(log.fields).map(([key,value]) => `${key}=${value}`).join(' ');
    line.append(time, document.createTextNode(`  ${log.level.padEnd(5)} ${log.message}${detail ? ' · ' + detail : ''}`)); return line;
  }));
  if (atEnd) container.scrollTop = container.scrollHeight;
}
$('clear-log').onclick = () => { logs=[]; renderLogs(); };
$('log-filter').onchange = renderLogs;
async function refresh() {
  const state = await invoke('snapshot', { after:cursor });
  running = state.running;
  $('phase').textContent = state.phase || 'Stopped';
  $('indicator').className = state.error ? 'failed' : running ? 'active' : '';
  if (state.error) showError(state.error);
  $('totals').textContent = state.updates ? `${state.updates.toLocaleString()} updates · ${bytes(state.bytes)}` : 'No transfers';
  if (state.logs.length) { cursor=state.logs.at(-1).id; logs=[...logs,...state.logs].slice(-600); renderLogs(); }
  lock();
}
function applyTheme(theme) {
  document.documentElement.dataset.theme = theme === 'dark' ? 'dark' : 'light';
}
const appearance = matchMedia('(prefers-color-scheme: dark)');
applyTheme(appearance.matches ? 'dark' : 'light');
appearance.addEventListener('change', event => applyTheme(event.matches ? 'dark' : 'light'));
if (window.__TAURI__?.event?.listen) {
  window.__TAURI__.event.listen('system-theme', event => applyTheme(event.payload)).catch(showError);
}
async function initialize() {
  try {
    const data = await invoke('bootstrap');
    if (data.theme) applyTheme(data.theme);
    settings = data.settings; profiles = data.profiles; addresses = data.addresses || [];
    $('platform').textContent = { macos:'macOS',linux:'Linux',windows:'Windows' }[data.platform] || data.platform;
    $('version').textContent = `v${data.version}`; $('pair').textContent = data.pair; $('pair').title = `Settings: ${data.config_path}`;
    render(); await refresh();
    const poll = async () => { try { await refresh(); } catch (error) { showError(error); } setTimeout(poll,700); };
    setTimeout(poll,700);
  } catch (error) { $('phase').textContent = 'Unavailable'; showError(error); }
}
initialize();
