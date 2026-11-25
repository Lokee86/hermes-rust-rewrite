// The part of @hermes/plugin-sdk a page can use outside the Electron renderer. Names a plugin
// imports that are not here become inert stand-ins, so one missing control does not lose the page.
(() => {
  const h = React.createElement
  const notice = text => {
    const box = document.getElementById('notice')
    box.textContent = String(text)
    box.hidden = false
    clearTimeout(box.timer)
    box.timer = setTimeout(() => { box.hidden = true }, 6000)
  }
  const unavailable = what => () => Promise.reject(new Error(`${what} is not available in this app`))
  const atom = value => ({ get: () => value, set: next => { value = next }, subscribe: () => () => {}, listen: () => () => {}, value })
  const pages = []
  let current = null
  const contribute = entry => {
    const item = { key: `${pages.length}`, title: entry.title ?? entry.data?.label ?? entry.data?.path ?? entry.id, path: entry.data?.path, area: entry.area, render: entry.render, order: entry.order ?? 0, when: entry.when }
    if (typeof entry.render === 'function' && entry.enabled !== false) pages.push(item)
    return () => { const at = pages.indexOf(item); if (at >= 0) pages.splice(at, 1); draw() }
  }
  const memory = new Map()
  const disposers = []
  const track = dispose => { disposers.push(dispose); return dispose }
  const context = {
    source: `plugin:${globalThis.__pluginId}`,
    register: contribute,
    registerMany: list => { const all = list.map(contribute); return () => all.forEach(stop => stop()) },
    registerSettingsPage: page => contribute({ ...page, area: 'settings.plugins' }),
    rest: unavailable('The plugin backend'),
    socket: () => () => {},
    onEvent: () => () => {},
    onDispose: track,
    setTimeout: (fn, ms) => { const id = setTimeout(fn, ms); return track(() => clearTimeout(id)) },
    setInterval: (fn, ms) => { const id = setInterval(fn, ms); return track(() => clearInterval(id)) },
    addEventListener: (target, type, fn, options) => { target.addEventListener(type, fn, options); return track(() => target.removeEventListener(type, fn, options)) },
    os: { notify: entry => notice(entry?.body ?? entry?.title ?? entry), openExternal: url => { window.open(url) }, revealPath: unavailable('Revealing files'), copy: unavailable('The clipboard') },
    pet: { say: notice },
    preview: { executeJavaScript: unavailable('Preview scripting') },
    storage: { get: (key, fallback) => (memory.has(key) ? memory.get(key) : fallback), set: (key, value) => memory.set(key, value), remove: key => memory.delete(key), delete: key => memory.delete(key), keys: () => [...memory.keys()] },
  }
  const box = tag => ({ children, className, style, onClick, title, disabled, placeholder, value, onChange, type, checked, href }) =>
    h(tag, { className, style, onClick, title, disabled, placeholder, value, onChange, type, checked, href }, children)
  const sdk = {
    host: { state: new Proxy({}, { get: (_, name) => atom(name === 'gateway' ? 'connected' : undefined) }), notify: entry => notice(entry?.message ?? entry?.title ?? entry),
      navigate: path => { const page = pages.find(item => item.path === path); if (page) { current = page.key; draw() } } },
    haptic: () => {},
    useValue: store => (store && typeof store.get === 'function' ? store.get() : store),
    ROUTES_AREA: 'routes', SIDEBAR_NAV_AREA: 'sidebar.nav', PANES_AREA: 'panes', PALETTE_AREA: 'palette', KEYBINDS_AREA: 'keybinds', THEMES_AREA: 'themes', SETTINGS_PLUGINS_AREA: 'settings.plugins',
    STATUSBAR_AREAS: { left: 'statusBar.left', right: 'statusBar.right' }, TITLEBAR_AREAS: { left: 'titleBar.left', center: 'titleBar.center', right: 'titleBar.right' },
    COMPOSER_AREAS: new Proxy({}, { get: (_, name) => `composer.${String(name)}` }),
    Button: box('button'), Input: box('input'), Textarea: box('textarea'), Badge: box('span'), Card: box('section'),
    cn: (...parts) => parts.flat(Infinity).filter(part => typeof part === 'string' && part).join(' '),
  }
  const standIn = name => {
    if (/^use[A-Z]/.test(name)) return () => undefined
    if (/^[A-Z][A-Z0-9_]+$/.test(name)) return name.toLowerCase()
    if (/^[A-Z]/.test(name)) return ({ children }) => h(React.Fragment, null, children)
    return () => undefined
  }
  const open = new Proxy(sdk, { get: (target, name) => (name in target ? target[name] : typeof name === 'string' ? standIn(name) : undefined) })
  const runtime = { jsx: (type, { children, ...props }, key) => h(type, key === undefined ? props : { ...props, key }, ...(children === undefined ? [] : [children])),
    jsxs: (type, { children, ...props }, key) => h(type, key === undefined ? props : { ...props, key }, ...[].concat(children ?? [])), Fragment: React.Fragment }
  globalThis.__import = name => {
    if (name === '@hermes/plugin-sdk') return open
    if (name === 'react') return Object.assign({ default: React }, React)
    if (name === 'react/jsx-runtime' || name === 'react/jsx-dev-runtime') return runtime
    throw new Error(`Desktop plugins can import only @hermes/plugin-sdk and react, not ${name}`)
  }
  class Guard extends React.Component {
    constructor(props) { super(props); this.state = {} }
    static getDerivedStateFromError(error) { return { error } }
    render() { return this.state.error ? h('p', { className: 'problem' }, `This page stopped: ${this.state.error.message ?? this.state.error}`) : this.props.children }
  }
  const root = ReactDOM.createRoot(document.getElementById('page'))
  const draw = () => {
    const shown = pages.filter(page => { try { return page.when ? page.when() : true } catch { return false } }).sort((a, b) => a.order - b.order)
    if (!shown.length) return root.render(h('p', { className: 'problem' }, 'This plugin adds no page.'))
    const page = shown.find(item => item.key === current) ?? shown[0]
    const View = page.render
    root.render(h(React.Fragment, null,
      shown.length > 1 && h('nav', null, shown.map(item => h('button', { key: item.key, 'aria-current': item === page, onClick: () => { current = item.key; draw() } }, item.title))),
      h(Guard, { key: page.key }, h(() => View({})))))
  }
  globalThis.__start = plugin => {
    try {
      if (!plugin || typeof plugin.register !== 'function') throw new Error('It does not export a plugin with register()')
      plugin.register(context)
      draw()
    } catch (error) { globalThis.__fail(error) }
  }
  globalThis.__fail = error => root.render(h('p', { className: 'problem' }, `This plugin did not load: ${error?.message ?? error}`))
})()
