# Security

Please report a vulnerability privately through [GitHub's security advisories](https://github.com/itsflownium/mercury-hermes-rust-rewrite/security/advisories/new), not in a public issue or pull request. Include the version, the platform, and the steps that show the problem. Do not include real keys, tokens or chats.

Only the latest release and `main` receive fixes.

## Checks on pull requests

| Check | Fails when |
| --- | --- |
| Dependencies | a pull request adds a dependency with a known vulnerability of moderate severity or more |
| Supply chain | a dependency comes from an unlisted source or has an unlisted licence ([`deny.toml`](deny.toml)) |
| Secrets | a commit contains a key or token |
| Commit identity | a commit was made under a computer's own account and host name |
| Workflows | a workflow is unpinned, open to injection, or has more access than it needs |
| CodeQL | the analysis of Rust, Python, JavaScript or a workflow cannot run; its findings are under **Security › Code scanning** |
| Lint (`build.yml`) | clippy finds a likely mistake, or `cargo audit` a vulnerability |

The lint job also audits Git-sourced `glib` against its crates.io advisories. It skips `RUSTSEC-2024-0429` only for the pinned `glib` 0.18.5 backport.

Pull requests from forks run with a read-only token and without secrets.
