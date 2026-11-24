#!/usr/bin/env node
// Stock Ink hook/renderer + Rust gateway live RPC conformance.
// In the pinned checkout install the workspace with its pinned pnpm and build ui-tui/packages/hermes-ink.
// Set HERMES_PYTHON to the interpreter used for the stock gateway.
// Use the ui-tui tsx loader with a file URL, for example:
// node --import file:///C:/path/to/stock/ui-tui/node_modules/tsx/dist/loader.mjs scripts/verify_stock_ink_composer.mjs --stock-checkout STOCK --gateway-exe GATEWAY
import assert from 'node:assert/strict'
import { execFileSync, spawn } from 'node:child_process'
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises'
import { createServer } from 'node:http'
import { PassThrough } from 'node:stream'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import { setTimeout as delay } from 'node:timers/promises'

const SHA = 'caafe5389ae585ef77387a962aee05b1bcf17029'
const REPLY = 'Stock Ink deterministic reply'
const marker = 'INK_INTERPOLATION_VALUE'
const stockAt = process.argv.indexOf('--stock-checkout')
const gatewayAt = process.argv.indexOf('--gateway-exe')
const stock = stockAt >= 0 ? resolve(process.argv[stockAt + 1]) : ''
const gatewayExe = gatewayAt >= 0 ? resolve(process.argv[gatewayAt + 1]) : ''
if (!stock || !gatewayExe) {
  console.error('Usage: node --import <stock ui-tui tsx loader> scripts/verify_stock_ink_composer.mjs --stock-checkout STOCK --gateway-exe GATEWAY')
  process.exit(2)
}
assert.equal(execFileSync('git', ['rev-parse', 'HEAD'], { cwd: stock, encoding: 'utf8' }).trim(), SHA,
  'the stock checkout must be at the pinned revision')
execFileSync('git', ['diff', '--quiet', 'HEAD', '--', 'ui-tui', 'apps/shared', 'package.json', 'package-lock.json'], { cwd: stock })
const modules = join(stock, 'ui-tui', 'node_modules')
const importFile = path => import(pathToFileURL(join(stock, path)).href)
const react = await import(pathToFileURL(join(modules, 'react/index.js')).href)
const ink = await import(pathToFileURL(join(modules, '@hermes/ink/index.js')).href)
const [{ GatewayClient }, { useSubmission }, { useQueue }, uiStore] = await Promise.all([
  importFile('ui-tui/src/gatewayClient.ts'),
  importFile('ui-tui/src/app/useSubmission.ts'),
  importFile('ui-tui/src/hooks/useQueue.ts'),
  importFile('ui-tui/src/app/uiStore.ts')
])
const root = await mkdtemp(join(tmpdir(), 'stock-ink-composer-'))
const profile = join(root, 'profile')
await mkdir(profile)
let server, provider, renderer, client
let providerCalls = 0
const calls = [], transcript = [], notices = [], providerRequests = []
const siblingText = 'Shared note from the parent directory'
const readSibling = process.platform === 'win32' ? 'type ..\\shared-note.txt' : 'cat ../shared-note.txt'
try {
  await writeFile(join(root, 'shared-note.txt'), siblingText + '\n')
  provider = createServer(async (req, res) => {
    const body = []
    for await (const chunk of req) body.push(chunk)
    providerRequests.push(JSON.parse(Buffer.concat(body).toString('utf8')))
    providerCalls++
    res.writeHead(200, { 'content-type': 'text/event-stream' })
    res.end([
      'data: ' + JSON.stringify({ choices: [{ index: 0, delta: { content: REPLY }, finish_reason: null }] }),
      '',
      'data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}',
      '',
      'data: [DONE]', '', ''
    ].join(String.fromCharCode(10)))
  })
  provider.listen(0, '127.0.0.1')
  await new Promise(resolveListen => provider.once('listening', resolveListen))
  const pass = new Set(['PATH','PATHEXT','SYSTEMROOT','SYSTEMDRIVE','WINDIR','COMSPEC','TEMP','TMP','TMPDIR',
    'HOME','USERPROFILE','APPDATA','LOCALAPPDATA','PROGRAMFILES','PROGRAMFILES(X86)','PROGRAMW6432','PYTHONPATH','VIRTUAL_ENV'])
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => pass.has(key.toUpperCase())))
  Object.assign(env, { HERMES_RUST_BASE_URL: 'http://127.0.0.1:' + provider.address().port + '/v1',
    HERMES_RUST_MODEL: 'stock-ink-fixture', HERMES_RUST_API_KEY: 'stock-ink-fixture-key', HERMES_HOME: profile })
  server = spawn(gatewayExe, ['gateway','run','--host','127.0.0.1','--port','0','--data-dir',profile],
    { env, stdio:['ignore','pipe','pipe'], windowsHide:true })
  let start = '', stderr = ''
  server.stdout.setEncoding('utf8'); server.stderr.setEncoding('utf8')
  server.stdout.on('data', chunk => { start += chunk })
  server.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-5000) })
  const until = Date.now() + 30000
  while (!start.length && Date.now() < until && server.exitCode === null) await delay(25)
  assert.equal(server.exitCode, null, 'gateway startup failed: ' + stderr)
  assert.ok(start.length, 'gateway startup timed out: ' + stderr)
  process.env.HERMES_HOME = profile
  process.env.HERMES_PYTHON_SRC_ROOT = stock
  delete process.env.HERMES_TUI_GATEWAY_URL
  delete process.env.HERMES_TUI_SIDECAR_URL

  client = new GatewayClient()
  const ready = new Promise((resolveReady, rejectReady) => {
    const timer = setTimeout(() => rejectReady(new Error('stock TUI gateway client timed out')), 30000)
    client.on('event', event => {
      if (event.type === 'gateway.ready') { clearTimeout(timer); resolveReady() }
      if (event.type === 'gateway.start_timeout') { clearTimeout(timer); rejectReady(new Error(event.payload?.stderr_tail ?? 'bootstrap failed')) }
    })
  })
  client.start(); client.drain(); await ready
  const request = client.request.bind(client)
  client.request = (method, params) => { calls.push({method,params}); return request(method,params) }
  const session = await client.request('session.create', { source:'tui', cwd:profile, title:'Stock Ink composer check' })
  const completed = new Promise(resolveComplete => client.on('event', event => {
    if (event.type === 'message.complete') resolveComplete(event)
  }))
  uiStore.resetUiState()
  uiStore.patchUiState({ sid:session.session_id, gatewayConnected:true, busy:false,
    info:{ model:'stock-ink-fixture', profile_name:'default', stored_session_id:session.session_id, skills:{}, tools:{} } })

  let submission, queue
  function Harness() {
    queue = useQueue()
    submission = useSubmission({
      appendMessage: msg => transcript.push(msg),
      composerActions:{ ...queue, prependQueue:queue.prependQ, takeQueue:queue.takeQ,
        collapsePaste: value => value, clearIn(){}, pushHistory(){}, setInput(){}, setQueueEdit(){} },
      composerRefs:{ queueRef:queue.queueRef, queueEditRef:queue.queueEditRef, tokensRef:{current:[]} },
      composerState:{ input:'', inputBuf:[], completions:[], compIdx:0, compReplace:null },
      gw:client, setLastUserMsg(){}, slashRef:{current(){}}, submitRef:{current(){}},
      sys:msg => notices.push(String(msg))
    })
    return react.createElement(ink.Text, null, 'Mounted stock useSubmission')
  }
  const stdout = Object.assign(new PassThrough(), { columns:100, rows:30, isTTY:false })
  renderer = ink.renderSync(react.createElement(Harness), {
    stdin:new PassThrough(), stdout, stderr:new PassThrough(), patchConsole:false
  })

  submission.submit('!echo ' + marker)
  await waitFor(() => uiStore.getUiState().status === 'ready' && notices.some(text => text.includes(marker)), '!command output')
  assert.ok(calls.some(c => c.method === 'shell.exec' && c.params.command === 'echo ' + marker))
  assert.ok(transcript.some(m => m.role === 'user' && m.text === '!echo ' + marker))
  assert.equal(providerCalls, 0)

  submission.submit('!exit 7')
  await waitFor(() => uiStore.getUiState().status === 'ready' && notices.some(text => text.includes('exit 7')), '!command exit output')
  assert.ok(calls.some(c => c.method === 'shell.exec' && c.params.command === 'exit 7'))
  assert.equal(providerCalls, 0)

  submission.submit('!' + readSibling)
  await waitFor(() => uiStore.getUiState().status === 'ready' && notices.some(text => text.includes(siblingText)), 'parent file capture')
  assert.equal(providerCalls, 0)

  const prompt = 'Answer with: ' + marker + ' / ' + siblingText
  submission.submit('Answer with: {!echo ' + marker + '} / {!' + readSibling + '}')
  await waitFor(() => providerCalls === 1, 'provider request after interpolation')
  assert.ok(calls.some(c => c.method === 'prompt.submit' && c.params.text === prompt))
  assert.ok(providerRequests.some(body => body.messages?.some(message => message.role === 'user' &&
    (message.content === prompt || (Array.isArray(message.content) && message.content.some(part => part.text === prompt))))),
    'the model request must contain the interpolated sibling file text')
  await Promise.race([completed, delay(15000).then(() => { throw new Error('stock TUI did not receive message.complete') })])
  const resumed = await client.request('session.resume', { session_id:session.session_id })
  assert.ok(resumed.messages.some(message => message.content === REPLY), 'deterministic assistant reply missing from session history')

  const submits = calls.filter(c => c.method === 'prompt.submit').length
  const providerBefore = providerCalls, noticeCount = notices.length
  await writeFile(join(profile, 'config.yaml'), 'terminal:\n  backend: docker\n')
  const blockedSession = await client.request('session.create', { source:'tui', cwd:profile, title:'Blocked interpolation check' })
  uiStore.patchUiState({ sid:blockedSession.session_id, busy:false, status:'ready',
    info:{ model:'stock-ink-fixture', profile_name:'default', stored_session_id:blockedSession.session_id, skills:{}, tools:{} } })
  submission.submit('Do not submit: {!echo harmless}')
  await waitFor(() => notices.length > noticeCount, 'blocked interpolation notice')
  assert.ok(notices.slice(noticeCount).some(text => /could not run .+input retained/i.test(text)))
  assert.equal(calls.filter(c => c.method === 'prompt.submit').length, submits)
  assert.equal(providerCalls, providerBefore)
  assert.ok(queue.queueRef.current.some(item => item.failed), 'failed interpolated draft was retained')

  console.log(JSON.stringify({ result:'pass', stock_sha:SHA, source:'ui-tui/src/app/useSubmission.ts',
    renderer:'@hermes/ink renderSync', gateway:'stock TUI GatewayClient + Rust local owner RPC',
    provider_calls:providerCalls, verified:['!command output/transcript','!command exit status',
      'successful {!command} prompt payload','parent file output with no provider call','parent file text in provider request','blocked {!command} retains draft and makes no prompt.submit'] }))
} finally {
  renderer?.unmount()
  client?.kill('stock-ink-composer-verification')
  if (server && server.exitCode === null) {
    server.kill()
    await Promise.race([new Promise(resolveExit => server.once('exit', resolveExit)), delay(2000)])
  }
  provider?.close()
  delete process.env.HERMES_HOME
  delete process.env.HERMES_PYTHON_SRC_ROOT
  await rm(root, { recursive:true, force:true, maxRetries:5, retryDelay:200 })
}
async function waitFor(check, what) {
  const deadline = Date.now() + 15000
  while (!check() && Date.now() < deadline) await delay(25)
  assert.ok(check(), 'timed out waiting for ' + what)
}
