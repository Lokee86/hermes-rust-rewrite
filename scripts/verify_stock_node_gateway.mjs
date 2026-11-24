// Run with Node 24 against a built Rust gateway and a Git checkout containing
// the pinned Hermes commit. The original TypeScript modules are not rewritten.
import assert from 'node:assert/strict'
import { execFileSync, spawn } from 'node:child_process'
import { randomBytes, scryptSync } from 'node:crypto'
import { once } from 'node:events'
import { existsSync } from 'node:fs'
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises'
import { createServer as createHttp } from 'node:http'
import { registerHooks } from 'node:module'
import { createConnection } from 'node:net'
import { basename, dirname, join, resolve } from 'node:path'
import { tmpdir } from 'node:os'
import { createInterface } from 'node:readline'
import { createServer as createTls, getCACertificates, setDefaultCACertificates } from 'node:tls'
import { setTimeout as delay } from 'node:timers/promises'
import { pathToFileURL } from 'node:url'

const revision = 'caafe5389ae585ef77387a962aee05b1bcf17029'
const [checkout, executable] = process.argv.slice(2)
if (!checkout || !executable) {
  console.error('Usage: node scripts/verify_stock_node_gateway.mjs STOCK_CHECKOUT RUST_GATEWAY_EXECUTABLE')
  process.exit(2)
}
const scratchParent = await realpath(tmpdir())
const scratch = await mkdtemp(join(scratchParent, 'hermes-stock-node-'))
const source = join(scratch, 'upstream')
const profile = join(scratch, 'profile')
const keyPath = join(scratch, 'fixture-key.pem')
const certificatePath = join(scratch, 'fixture-certificate.pem')
let gateway
let desktop
let tui
let model
let proxy
const connections = new Set()

function obtainStockSources() {
  const archive = execFileSync('git', [
    '-C', resolve(checkout), 'archive', '--format=tar', revision, 'apps/shared/src', 'apps/desktop/electron'
  ], { maxBuffer: 64 * 1024 * 1024 })
  execFileSync('tar', ['-xf', '-', '-C', source], { input: archive })
}

function makeCertificate() {
  let openssl = process.env.OPENSSL || 'openssl'
  if (process.platform === 'win32' && !process.env.OPENSSL) {
    const gitExec = execFileSync('git', ['--exec-path'], { encoding: 'utf8' }).trim()
    const bundled = resolve(gitExec, '..', '..', '..', 'usr', 'bin', 'openssl.exe')
    if (existsSync(bundled)) openssl = bundled
  }
  execFileSync(openssl, [
    'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
    '-subj', '/CN=127.0.0.1', '-addext', 'subjectAltName=IP:127.0.0.1',
    '-keyout', keyPath, '-out', certificatePath
  ], { stdio: ['ignore', 'pipe', 'pipe'] })
}

try {
  await mkdir(source)
  await mkdir(profile)
  obtainStockSources()
  makeCertificate()
  const sourceUrl = pathToFileURL(source + '/').href
  registerHooks({
    resolve(specifier, context, nextResolve) {
      try {
        return nextResolve(specifier, context)
      } catch (error) {
        if (!specifier.startsWith('.') || !context.parentURL?.startsWith(sourceUrl)
            || !context.parentURL.endsWith('.ts')) throw error
        const resolved = specifier.endsWith('.js') ? specifier.slice(0, -3) + '.ts' : specifier + '.ts'
        return nextResolve(resolved, context)
      }
    }
  })
  const { JsonRpcGatewayClient } = await import(sourceUrl + 'apps/shared/src/json-rpc-gateway.ts')
  const { GatewayClient } = await import(sourceUrl + 'apps/shared/src/node/gateway-client.ts')
  const { resolveTestWsUrl, authModeFromStatus, cookiesHaveSession } =
    await import(sourceUrl + 'apps/desktop/electron/connection-config.ts')
  const { mintGatewayWsTicket } = await import(sourceUrl + 'apps/desktop/electron/oauth-rest-request.ts')

  const [key, cert] = await Promise.all([readFile(keyPath), readFile(certificatePath)])
  setDefaultCACertificates([...getCACertificates('default'), cert.toString()])
  let listenerPort
  proxy = createTls({ key, cert }, downstream => {
    const upstream = createConnection({ host: '127.0.0.1', port: listenerPort })
    connections.add(downstream)
    connections.add(upstream)
    downstream.on('close', () => { connections.delete(downstream); upstream.destroy() })
    upstream.on('close', () => { connections.delete(upstream); downstream.destroy() })
    downstream.on('error', () => upstream.destroy())
    upstream.on('error', () => downstream.destroy())
    downstream.pipe(upstream).pipe(downstream)
  })
  proxy.listen(0, '127.0.0.1')
  await once(proxy, 'listening')
  const origin = 'https://127.0.0.1:' + proxy.address().port

  let providerCalls = 0
  const reply = 'Stock client live reply'
  model = createHttp(async (request, response) => {
    for await (const _ of request) {}
    providerCalls++
    response.writeHead(200, { 'content-type': 'text/event-stream' })
    response.end('data: ' + JSON.stringify({ choices: [{ index: 0, delta: { content: reply }, finish_reason: null }] })
      + '\n\ndata: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n')
  })
  model.listen(0, '127.0.0.1')
  await once(model, 'listening')

  const password = randomBytes(24).toString('base64url')
  const deployment = randomBytes(32).toString('base64url')
  const salt = randomBytes(16)
  const hash = ['scrypt', '16384', '8', '1', salt.toString('base64'),
    scryptSync(password, salt, 32, { N: 16384, r: 8, p: 1 }).toString('base64')].join('$')
  await writeFile(join(profile, 'config.yaml'), 'webapp:\n  public_url: ' + origin
    + '\ndashboard:\n  basic_auth:\n    username: fixture\n    password_hash: ' + hash + '\n')
  const launchEnvironment = new Set([
    'PATH', 'PATHEXT', 'SYSTEMROOT', 'SYSTEMDRIVE', 'WINDIR', 'COMSPEC', 'TEMP', 'TMP', 'TMPDIR',
    'HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA', 'PROGRAMFILES', 'PROGRAMFILES(X86)', 'PROGRAMW6432'
  ])
  const environment = Object.fromEntries(
    Object.entries(process.env).filter(([name]) => launchEnvironment.has(name.toUpperCase()))
  )
  Object.assign(environment, {
    HERMES_DASHBOARD_SESSION_TOKEN: deployment,
    HERMES_DASHBOARD_PUBLIC_URL: origin,
    HERMES_RUST_BASE_URL: 'http://127.0.0.1:' + model.address().port + '/v1',
    HERMES_RUST_MODEL: 'fixture-stock-model',
    HERMES_RUST_API_KEY: randomBytes(24).toString('base64url')
  })
  gateway = spawn(resolve(executable), ['gateway', 'run', '--port', '0', '--ephemeral-token', '--data-dir', profile], {
    env: environment, stdio: ['ignore', 'pipe', 'inherit']
  })
  const output = createInterface({ input: gateway.stdout })
  const [line] = await Promise.race([
    once(output, 'line'),
    once(gateway, 'exit').then(([code]) => { throw new Error('Gateway exited with code ' + code) }),
    delay(25000, null, { ref: false }).then(() => { throw new Error('Gateway startup timed out') })
  ])
  listenerPort = Number(new URL(JSON.parse(line).http_url).port)

  const get = async (path, options = {}) => {
    const response = await fetch(origin + path, { ...options, signal: AbortSignal.timeout(25000) })
    const body = await response.json()
    assert.ok(response.ok, 'Unexpected HTTP status ' + response.status + ' on ' + path)
    return body
  }
  const status = await get('/api/status')
  assert.equal(authModeFromStatus(status), 'oauth')
  const login = await fetch(origin + '/auth/password-login', {
    method: 'POST',
    headers: { 'content-type': 'application/json', origin },
    body: JSON.stringify({ provider: 'basic', username: 'fixture', password }),
    signal: AbortSignal.timeout(25000)
  })
  assert.equal(login.status, 200)
  await login.json()
  const cookie = login.headers.getSetCookie()[0].split(';')[0]
  const [cookieName, cookieValue] = cookie.split('=')
  assert.ok(cookiesHaveSession([{ name: cookieName, value: cookieValue }]))
  const headers = { Cookie: cookie }
  const profiles = await get('/api/profiles', { headers })
  assert.equal(profiles.profiles[0].name, 'default')

  const mintTicket = url => mintGatewayWsTicket(url, {
    ensureNativeAccessToken: async () => null,
    fetchJson: async () => { throw new Error('Unexpected bearer selection') },
    fetchJsonViaOauthSession: (absolute, options) =>
      get(new URL(absolute).pathname, { method: options.method, headers })
  })
  const desktopTarget = await resolveTestWsUrl(origin, 'oauth', null, { mintTicket })
  desktop = new JsonRpcGatewayClient({ connectTimeoutMs: 5000, heartbeatIntervalMs: 0, heartbeatDeadlineMs: 0 })
  await desktop.connect(desktopTarget)
  const chat = await desktop.request('session.create', { source: 'gui', title: 'Stock client live conformance' })
  const sid = chat.session_id
  assert.equal(typeof sid, 'string')
  const completed = new Promise((done, reject) => {
    desktop.onAny(event => { if (event.type === 'message.complete') done(event) })
    delay(8000, null, { ref: false }).then(() => reject(new Error('Stock client did not receive completion')))
  })
  await desktop.request('prompt.submit', {
    session_id: sid, text: 'Respond using the deterministic fixture', submission_id: 'stock-node-conformance'
  })
  await completed
  const latest = await get('/api/sessions/' + sid + '/messages?order=latest&limit=1', { headers })
  const older = await get('/api/sessions/' + sid + '/messages?order=latest&limit=1&offset=1', { headers })
  assert.equal(latest.messages[0].content, reply)
  assert.equal(older.messages[0].role, 'user')
  const sidebar = await get('/api/profiles/sessions/sidebar?recents_profile=default', { headers })
  assert.equal(sidebar.recents.sessions[0].id, sid)
  const form = new FormData()
  form.append('file', new Blob(['fixture attachment'], { type: 'text/plain' }), 'fixture.txt')
  const uploaded = await get('/api/chat/file-upload?profile=default', { method: 'POST', headers, body: form })
  assert.equal(uploaded.size, 18)
  assert.ok(uploaded.staged_upload.install_id)

  const runtime = await desktop.request('runtime.describe', {})
  tui = new GatewayClient({
    source: 'tui', requestTimeoutMs: 5000, startupTimeoutMs: 5000,
    connect: async () => {
      const minted = await get('/api/auth/ws-ticket?profile=default', {
        method: 'POST', headers: { Authorization: 'Bearer ' + deployment }
      })
      return {
        url: origin.replace('https:', 'wss:') + '/api/ws',
        protocols: ['hermes-gateway-v1', 'hermes-gateway-ticket.' + minted.ticket],
        profile_id: runtime.profile_id, instance_id: runtime.instance_id
      }
    }
  })
  const resumed = await tui.request('session.resume', { session_id: sid })
  assert.equal(resumed.messages.at(-1).content, reply)
  assert.equal(resumed.messages.at(-1).message_id, latest.messages[0].message_id)
  assert.equal(providerCalls, 1)
  console.log(JSON.stringify({
    revision, platform: process.platform, node: process.version, providerCalls,
    verified: ['password cookie recognition', 'Desktop ticket selection and JSON-RPC streaming',
      'originless canonical attach and resume', 'profile sidebar and paged history', 'authenticated attachment staging']
  }))
} finally {
  desktop?.close()
  tui?.kill()
  for (const socket of connections) socket.destroy()
  proxy?.close()
  model?.close()
  if (gateway?.pid && gateway.exitCode === null && gateway.signalCode === null) {
    const exited = once(gateway, 'exit')
    gateway.kill()
    await exited
  }
  assert.equal(dirname(scratch), scratchParent)
  assert.ok(basename(scratch).startsWith('hermes-stock-node-'))
  await rm(scratch, { recursive: true, force: true, maxRetries: 3 })
}
