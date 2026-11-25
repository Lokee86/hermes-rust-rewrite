//! Pages that plugins add to the desktop (`desktop/plugin.js` in an installed plugin, or
//! `desktop-plugins/<id>/plugin.js`), shown in the preview webview.
//!
//! Electron gives such a plugin the whole renderer. Here it is guest content: one generated file
//! with no network, no other files, no IPC and an empty store (webview.rs). It gets React and the
//! page part of the plugin SDK (assets/plugin-page/host.js).
use iced::Color;
use regex::Regex;
use std::{fmt::Write, fs, path::{Path, PathBuf}, sync::LazyLock};

const REACT: &str = include_str!("../assets/plugin-page/react.production.min.js");
const REACT_DOM: &str = include_str!("../assets/plugin-page/react-dom.production.min.js");
const HOST: &str = include_str!("../assets/plugin-page/host.js");
const MAX_SOURCE: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Page {
    pub id: String,
    pub source: PathBuf,
}

fn valid(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && !id.starts_with('.')
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The profile's enabled plugins that have a desktop part.
pub(super) fn discover(root: &Path, installed: &[hermes_engine::plugins::Installed]) -> Vec<Page> {
    let mut pages: Vec<Page> = installed.iter().filter(|plugin| plugin.enabled && valid(&plugin.name))
        .map(|plugin| Page { id: plugin.name.clone(), source: root.join("plugins").join(&plugin.name).join("desktop/plugin.js") })
        .collect();
    for entry in fs::read_dir(root.join("desktop-plugins")).into_iter().flatten().flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        if valid(&id) && !pages.iter().any(|page| page.id == id) {
            pages.push(Page { source: entry.path().join("plugin.js"), id });
        }
    }
    pages.retain(|page| page.source.is_file());
    pages.sort_by(|a, b| a.id.cmp(&b.id));
    pages
}

/// Turn the plugin's module into a script: its three possible imports and its default export.
fn script(source: &str) -> Result<String, String> {
    static IMPORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?m)^[ \t]*import\s+(?:([A-Za-z_$][\w$]*)\s*,?\s*)?(?:\*\s+as\s+([A-Za-z_$][\w$]*)\s*)?(?:\{([^}]*)\}\s*)?from\s*['"]([^'"]+)['"][ \t]*;?"#).unwrap());
    static BARE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?m)^[ \t]*import\s*['"]([^'"]+)['"]"#).unwrap());
    static DEFAULT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^[ \t]*export\s+default\s+").unwrap());
    static NAMED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^[ \t]*export\s+(const|let|var|function|class|async)\b").unwrap());
    if let Some(found) = BARE.captures(source) {
        return Err(format!("It imports {}, which desktop plugins cannot", &found[1]));
    }
    if DEFAULT.find_iter(source).count() != 1 {
        return Err("It needs exactly one default export".into());
    }
    let text = IMPORT.replace_all(source, |found: &regex::Captures| {
        let module = serde_json::to_string(&found[4]).unwrap_or_default();
        let mut out = String::new();
        if let Some(name) = found.get(1) { let _ = write!(out, "const {} = __import({module}).default;", name.as_str()); }
        if let Some(name) = found.get(2) { let _ = write!(out, "const {} = __import({module});", name.as_str()); }
        if let Some(names) = found.get(3) {
            let names = names.as_str().split(',').map(str::trim).filter(|name| !name.is_empty())
                .map(|name| name.split_once(" as ").map_or_else(|| name.to_owned(), |(from, to)| format!("{}: {}", from.trim(), to.trim())))
                .collect::<Vec<_>>().join(", ");
            let _ = write!(out, "const {{ {names} }} = __import({module});");
        }
        // Keep line numbers the same in error messages.
        out + &"\n".repeat(found[0].matches('\n').count())
    });
    let text = NAMED.replace_all(&text, "$1");
    Ok(DEFAULT.replace(&text, "const __plugin = ").into_owned())
}

/// Inside `<script>`, only these could end it.
fn inline(text: &str) -> String {
    text.replace("</", "<\\/").replace("<!--", "<\\!--")
}

fn css(color: Color) -> String {
    let part = |value: f32| (value * 255.0).round() as u8;
    format!("rgb({} {} {})", part(color.r), part(color.g), part(color.b))
}

/// Write the page's file and return it. `colors` are the background, foreground, muted and border.
pub(super) fn build(data: &Path, page: &Page, colors: [Color; 4]) -> Result<PathBuf, String> {
    let fail = |error: std::io::Error| format!("Could not open the plugin page: {error}");
    if fs::metadata(&page.source).map_err(fail)?.len() > MAX_SOURCE {
        return Err("The plugin's page is too large".into());
    }
    let source = fs::read_to_string(&page.source).map_err(fail)?;
    let [background, foreground, muted, border] = colors.map(css);
    let id = serde_json::to_string(&page.id).unwrap_or_default();
    let body = match script(&source) {
        // A plugin that fails to parse or run must not take the page's own code with it.
        Ok(text) => format!("<script>window.onerror = message => {{ __fail(message); return true }}</script>\n<script>(() => {{\n{}\n;__start(__plugin)\n}})()</script>", inline(&text)),
        Err(error) => format!("<script>__fail({})</script>", inline(&serde_json::to_string(&error).unwrap_or_default())),
    };
    let html = format!("<!doctype html><html><head><meta charset=\"utf-8\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:\">\
<style>:root{{--ui-text:{foreground};--ui-text-tertiary:{muted};--ui-border:{border}}}\
body{{margin:0;padding:16px;background:{background};color:{foreground};font:14px system-ui,sans-serif}}\
nav{{display:flex;gap:8px;margin-bottom:16px}}button,input,textarea,select{{font:inherit;color:inherit;background:transparent;border:1px solid {border};border-radius:6px;padding:4px 10px}}\
button[aria-current=true]{{border-color:{foreground}}}.problem{{color:{muted}}}\
#notice{{position:fixed;right:16px;bottom:16px;padding:8px 12px;border:1px solid {border};border-radius:8px;background:{background}}}</style></head>\
<body><div id=\"page\"></div><div id=\"notice\" hidden></div>\
<script>globalThis.__pluginId = {id}</script><script>{}</script><script>{}</script><script>{}</script>\n{body}</body></html>",
        inline(REACT), inline(REACT_DOM), inline(HOST));
    // A folder of its own: the webview lets a page read its folder.
    let folder = data.join("plugin-pages").join(&page.id);
    fs::create_dir_all(&folder).map_err(fail)?;
    let file = folder.join("index.html");
    let temp = tempfile::NamedTempFile::new_in(&folder).map_err(fail)?;
    fs::write(temp.path(), html).map_err(fail)?;
    temp.persist(&file).map_err(|error| fail(error.error))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLUGIN: &str = r#"import { host, haptic as tap,
  useValue } from '@hermes/plugin-sdk'
import React, { useState } from "react"
import * as runtime from 'react/jsx-runtime';

export const VERSION = 1
function Page() { const s = '</script><!--'; return runtime.jsx('p', { children: s }) }

export default {
  id: 'hello',
  register(ctx) { ctx.register({ id: 'page', area: 'routes', title: 'Hello', data: { path: '/hello' }, render: () => runtime.jsx(Page, {}) }) }
}
"#;

    #[test]
    fn a_module_becomes_a_script_with_the_same_lines() {
        let text = script(PLUGIN).unwrap();
        assert!(text.contains(r#"const { host, haptic: tap, useValue } = __import("@hermes/plugin-sdk");"#), "{text}");
        assert!(text.contains(r#"const React = __import("react").default;const { useState } = __import("react");"#));
        assert!(text.contains(r#"const runtime = __import("react/jsx-runtime");"#));
        assert!(text.contains("const __plugin = {") && text.contains("\nconst VERSION = 1") && !text.contains("export"));
        assert_eq!(text.lines().count(), PLUGIN.lines().count());
        assert!(script("import 'node:fs'\nexport default {}").is_err());
        assert!(script("const x = 1").is_err());
    }

    #[test]
    fn pages_come_from_enabled_plugins_and_are_closed_off() {
        let root = tempfile::tempdir().unwrap();
        for folder in ["plugins/on/desktop", "plugins/off/desktop", "desktop-plugins/hello", "desktop-plugins/..evil"] {
            fs::create_dir_all(root.path().join(folder)).unwrap();
            fs::write(root.path().join(folder).join("plugin.js"), PLUGIN).unwrap();
        }
        let plugin = |name: &str, enabled| hermes_engine::plugins::Installed { name: name.into(), enabled, ..Default::default() };
        let pages = discover(root.path(), &[plugin("on", true), plugin("off", false), plugin("no-page", true)]);
        assert_eq!(pages.iter().map(|page| page.id.as_str()).collect::<Vec<_>>(), ["hello", "on"]);
        let file = build(root.path(), &pages[0], [Color::WHITE, Color::BLACK, Color::BLACK, Color::BLACK]).unwrap();
        let html = fs::read_to_string(file).unwrap();
        assert!(html.contains("default-src 'none'") && !html.contains("connect-src"));
        assert!(html.contains(r"'<\/script><\!--'"), "plugin text cannot end its script");
        // The id, React, React DOM, the host, the error handler and the plugin; React itself contains "<script>".
        assert_eq!(html.matches("</script>").count(), 6);
    }

    /// Writes a page for checking in a browser: `HERMES_RUST_DATA_DIR=<dir> cargo test -- --ignored writes_a_demo_plugin_page`
    #[test]
    #[ignore = "writes a page for manual verification"]
    fn writes_a_demo_plugin_page() {
        let root = PathBuf::from(std::env::var_os("HERMES_RUST_DATA_DIR").unwrap());
        let source = root.join("demo.js");
        fs::write(&source, r#"
import { host, useValue, MissingControl, useMissing } from '@hermes/plugin-sdk'
import { useState } from 'react'
import { jsx, jsxs } from 'react/jsx-runtime'
function Counter() {
  const [n, set] = useState(0)
  return jsxs('div', { children: [jsx('h1', { children: 'Hello from a plugin' }), jsx('p', { children: 'gateway: ' + useValue(host.state.gateway) }),
    jsx(MissingControl, { children: jsx('em', { children: 'kept' }) }), jsx('button', { id: 'more', onClick: () => { set(n + 1); host.notify({ message: 'clicked ' + (n + 1) }) }, children: 'clicked ' + n })] })
}
export default { id: 'demo', register(ctx) { ctx.register({ id: 'p', area: 'routes', title: 'Counter', data: { path: '/c' }, render: () => jsx(Counter, {}) });
  ctx.register({ id: 'q', area: 'panes', title: 'Broken', render: () => { throw new Error('boom') } }) } }
"#).unwrap();
        let page = Page { id: "demo".into(), source };
        println!("{}", build(&root, &page, [Color::WHITE, Color::BLACK, Color::from_rgb8(120, 120, 120), Color::from_rgb8(200, 200, 200)]).unwrap().display());
    }
}
