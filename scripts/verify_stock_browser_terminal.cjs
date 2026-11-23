#!/usr/bin/env node
'use strict';

/**
 * Run the pinned stock Hermes browser terminal client against a local Rust gateway and Core:
 *   node --experimental-transform-types scripts/verify_stock_browser_terminal.cjs \
 *     --stock-checkout C:\\!bin\\workspace\\hermes-agent \
 *     --gateway-exe C:\\path\\to\\hermes-gateway.exe \
 *     --core-exe C:\\path\\to\\herdr-core.exe
 *
 * This imports the unmodified pinned browser-terminal.ts module with synthetic browser globals and
 * storage. Its websocketUrl callback is the test adapter: it uses the stock session header and
 * socket-ticket route, then returns the host-terminal WebSocket URL.
 */
const fs = require('node:fs');
const path = require('node:path');
const { spawn, execFileSync } = require('node:child_process');
const { createInterface } = require('node:readline');
const { pathToFileURL } = require('node:url');

const PINNED_STOCK_SHA = 'caafe5389ae585ef77387a962aee05b1bcf17029';

function parseArgs(argv) {
  const result = {};
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') {
      console.log('Required: --stock-checkout PATH --gateway-exe PATH --core-exe PATH');
      process.exit(0);
    }
    if (!['--stock-checkout', '--gateway-exe', '--core-exe'].includes(arg)) {
      throw new Error('Unknown argument: ' + arg);
    }
    const value = argv[++i];
    if (!value || value.startsWith('--')) throw new Error(arg + ' requires a path');
    result[arg] = path.resolve(value);
  }
  for (const arg of ['--stock-checkout', '--gateway-exe', '--core-exe']) {
    if (!result[arg]) throw new Error('Missing required argument: ' + arg);
  }
  return {
    stockCheckout: result['--stock-checkout'],
    gatewayExe: result['--gateway-exe'],
    coreExe: result['--core-exe']
  };
}

function assertInputs({ stockCheckout, gatewayExe, coreExe }) {
  if (!fs.existsSync(gatewayExe) || !fs.statSync(gatewayExe).isFile()) {
    throw new Error('Gateway executable not found: ' + gatewayExe);
  }
  if (!fs.existsSync(coreExe) || !fs.statSync(coreExe).isFile()) {
    throw new Error('Core executable not found: ' + coreExe);
  }
  const commit = execFileSync('git', ['rev-parse', PINNED_STOCK_SHA + '^{commit}'], {
    cwd: stockCheckout, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe']
  }).trim();
  if (commit !== PINNED_STOCK_SHA) {
    throw new Error('Pinned stock source commit unavailable: ' + commit);
  }
}

function createNodeModule(root, stockCheckout) {
  const moduleDir = path.join(root, 'module');
  const sharedDir = path.join(moduleDir, 'node_modules', '@hermes', 'shared');
  fs.mkdirSync(sharedDir, { recursive: true });
  fs.writeFileSync(path.join(moduleDir, 'package.json'), JSON.stringify({ type: 'module' }));
  fs.writeFileSync(path.join(sharedDir, 'package.json'), JSON.stringify({
    type: 'module', exports: './index.js'
  }));
  fs.writeFileSync(path.join(sharedDir, 'index.js'), [
    "export const LOCAL_CONNECTION_ID = 'local';",
    "export function isGatewayReauthRequired(error) { return error && error.name === 'BrowserReauthRequiredError'; }",
    'export function isStableOpen(openedAt) { return openedAt !== null && Date.now() - openedAt > 1000; }',
    'export function reconnectBackoffDelayMs(retry, { baseDelayMs, capMs }) { return Math.min(capMs, baseDelayMs * 2 ** retry); }'
  ].join('\n'));

  const source = execFileSync('git', [
    'show', PINNED_STOCK_SHA + ':apps/desktop/src/lib/browser-terminal.ts'
  ], { cwd: stockCheckout, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
  // Node's ESM resolver needs the extension, while Vite resolves this stock import without one.
  fs.writeFileSync(path.join(moduleDir, 'browser-terminal.ts'),
    source.replace("from './storage'", "from './storage.ts'"));
  const storageModule = [
    "export function readJson(key) { try { return JSON.parse(globalThis.__stockStorage.get(key) || 'null'); } catch { return null; } }",
    'export function writeJson(key, value) { if (value === null) globalThis.__stockStorage.delete(key); else globalThis.__stockStorage.set(key, JSON.stringify(value)); }'
  ].join('\n');
  fs.writeFileSync(path.join(moduleDir, 'storage.ts'), storageModule);
  globalThis.__stockStorage = new Map();
  globalThis.window = {
    location: { origin: 'http://127.0.0.1' },
    setTimeout, clearTimeout,
    addEventListener() {},
    hermesDesktop: null
  };
  return import(pathToFileURL(path.join(moduleDir, 'browser-terminal.ts')).href);
}

function minimalEnvironment(dataDir, coreExe) {
  const env = {};
  for (const name of [
    'PATH', 'SYSTEMROOT', 'WINDIR', 'TEMP', 'TMP', 'LOCALAPPDATA', 'APPDATA', 'USERPROFILE'
  ]) {
    if (process.env[name]) env[name] = process.env[name];
  }
  Object.assign(env, {
    HERMES_HOME: dataDir,
    HERMES_RUST_DATA_DIR: dataDir,
    HERMES_HERDR_CORE: coreExe
  });
  return env;
}

async function waitForOutput(readline, child, timeoutMs) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Timed out waiting for gateway startup')), timeoutMs);
    readline.once('line', line => {
      clearTimeout(timer);
      try { resolve(JSON.parse(line)); } catch (error) { reject(error); }
    });
    child.once('error', error => { clearTimeout(timer); reject(error); });
    child.once('exit', code => {
      clearTimeout(timer);
      reject(new Error('Gateway exited before startup (code ' + code + ')'));
    });
  });
}

async function waitForText(read, text, timeoutMs, message) {
  const deadline = Date.now() + timeoutMs;
  while (!read().includes(text) && Date.now() < deadline) {
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  if (!read().includes(text)) throw new Error(message);
}

async function run() {
  const options = parseArgs(process.argv.slice(2));
  assertInputs(options);
  const repoRoot = path.resolve(__dirname, '..');
  const targetDir = path.join(repoRoot, 'target');
  fs.mkdirSync(targetDir, { recursive: true });
  const root = fs.mkdtempSync(path.join(targetDir, 'stock-browser-terminal-'));
  const dataDir = path.join(root, 'profile');
  fs.mkdirSync(dataDir);
  let owner;
  let readline;
  let api;
  let terminalId;
  try {
    const { createBrowserTerminal } = await createNodeModule(root, options.stockCheckout);
    owner = spawn(options.gatewayExe, [
      'gateway', 'run', '--host', '127.0.0.1', '--port', '0', '--data-dir', dataDir
    ], { env: minimalEnvironment(dataDir, options.coreExe), stdio: ['ignore', 'pipe', 'pipe'] });
    let ownerError = '';
    owner.stderr.on('data', chunk => { ownerError = (ownerError + chunk.toString()).slice(-4000); });
    readline = createInterface({ input: owner.stdout });
    const info = await waitForOutput(readline, owner, 30000);
    if (!info.token_file || !info.http_url) throw new Error('Gateway omitted local token or URL');
    const token = fs.readFileSync(info.token_file, 'utf8').trim();
    if (!token) throw new Error('Gateway local token file was empty');
    const origin = new URL(info.http_url).origin;
    globalThis.window.location.origin = origin;
    const bootstrap = { authRequired: false, basePath: '', privateSession: true, token };

    const websocketUrl = async profile => {
      const response = await fetch(origin + '/api/webapp/socket-ticket?socket=host-terminal', {
        method: 'POST',
        headers: { 'X-Hermes-Session-Token': bootstrap.token }
      });
      const body = await response.json();
      if (!response.ok || !body.ticket) {
        throw new Error('Host-terminal ticket request failed: ' + response.status);
      }
      const url = new URL('/api/host-terminal', origin);
      url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
      url.searchParams.set('ticket', body.ticket);
      if (profile) url.searchParams.set('profile', profile);
      return url.toString();
    };

    api = createBrowserTerminal({
      basePath: '',
      currentProfile: () => null,
      currentConnection: () => 'local',
      websocketUrl,
      defaultCwd: async () => dataDir
    });
    const marker = 'STOCK-RESULT=782313';
    const first = await api.start({
      cols: 90, rows: 28, cwd: dataDir, restoreKey: 'stock-host-terminal'
    });
    terminalId = first.id;
    let output = '';
    const stopFirst = api.onData(first.id, chunk => { output += chunk; });
    if (!await api.write(first.id, 'set HERMES_WEBAPP_PROBE=782313\recho STOCK-RESULT=%HERMES_WEBAPP_PROBE%\r')) {
      throw new Error('Stock terminal write was rejected');
    }
    await waitForText(() => output, marker, 12000, 'Pinned Core PTY output was not received');
    await api.detach(first.id);

    const second = await api.start({
      restoreKey: 'stock-host-terminal', resumeOnly: true, cols: 90, rows: 28
    });
    terminalId = second.id;
    if (!second.persistent || !await api.attach(second.id)) {
      throw new Error('Stock terminal did not resume the saved shell');
    }
    let replay = '';
    const stopSecond = api.onData(second.id, chunk => { replay += chunk; });
    await waitForText(() => replay, marker, 12000, 'Reattached PTY did not repaint the original command result');
    const saved = [...globalThis.__stockStorage.entries()].find(([key]) => key.startsWith('hermes.webapp.terminal.v1:'));
    if (!saved) throw new Error('Stock client did not persist a terminal identity');
    await api.dispose(second.id);
    // Reinsert the stale bookmark to check that the gateway also ended the terminal.
    globalThis.__stockStorage.set(...saved);
    try {
      const unexpected = await api.start({ restoreKey: 'stock-host-terminal', resumeOnly: true });
      terminalId = unexpected.id;
      throw new Error('The closed PTY still accepted a new viewer');
    } catch (error) {
      if (error.signal !== 'expired') throw error;
    }
    terminalId = undefined;
    stopFirst();
    stopSecond();
    console.log(JSON.stringify({
      result: 'pass',
      stock_sha: PINNED_STOCK_SHA,
      source_module: 'apps/desktop/src/lib/browser-terminal.ts',
      core: path.basename(options.coreExe),
      authenticated_session: 'temporary local token',
      terminal_output: true,
      persisted_resume: true,
      explicit_close: true
    }));
  } finally {
    if (api && terminalId) {
      try { await api.dispose(terminalId); } catch {}
    }
    if (readline) readline.close();
    if (owner && owner.exitCode === null) {
      owner.kill();
      await new Promise(resolve => {
        const timer = setTimeout(resolve, 2000);
        owner.once('exit', () => { clearTimeout(timer); resolve(); });
      });
    }
    const host = path.join(dataDir, 'terminal-host');
    const identity = path.join(host, 'identity');
    if (fs.existsSync(identity)) {
      const hash = require('node:crypto').createHash('sha256').update(fs.readFileSync(identity)).digest('hex');
      execFileSync(options.coreExe, ['server', 'stop'], { windowsHide: true, timeout: 5000, cwd: host,
        env: { ...minimalEnvironment(dataDir, options.coreExe), HC_SESSION: 'hm-' + hash.slice(0, 24),
          XDG_STATE_HOME: path.join(host, 'state'), XDG_CONFIG_HOME: path.join(host, 'config') } });
    }
    fs.rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  }
}

run().catch(error => {
  console.error(error && error.stack ? error.stack : String(error));
  process.exitCode = 1;
});
