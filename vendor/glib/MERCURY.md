# Mercury’s glib patch

This is the crates.io `glib` 0.18.5 package from gtk-rs revision `42b9caf98e03ded086362d9653ca58fe94dc8658`. The original archive SHA-256 is `233daaf6e83ae6a12a52055f568f9d7cf4671dabb78ff9560ab6da230ce00ee5`. The [MIT license](LICENSE) and [copyright notice](COPYRIGHT) ship with Mercury.

Mercury’s Linux webview uses the GTK3 bindings, which require glib 0.18. This copy carries the upstream `VariantStrIter` fix for `RUSTSEC-2024-0429` (`GHSA-wrw7-89jp-8q8g`). The reported package version remains 0.18.5, so version-based scanners may still show the advisory. [Upstream fix](https://github.com/gtk-rs/gtk-rs-core/pull/1343)
