# Nyx

Nyx is an experimental, privacy-minded text browser written in Rust. It presents web pages in a keyboard-driven terminal interface and keeps its core parsing and navigation logic separate from platform-specific networking.

> **Project status:** early prototype. Nyx is useful for experimenting with text-first browsing, but it is not a complete or hardened web browser. It does not execute JavaScript and cannot reliably sign in to modern JavaScript-heavy sites.

## Features

- Search through a SearXNG JSON API, with a local instance as the default.
- Navigate directly to HTTP and HTTPS URLs.
- Convert HTML pages into readable text, links, basic forms, and image references.
- Submit supported HTML GET/POST forms. Session cookies can be stored in an encrypted local vault.
- View PNG, JPEG, WebP, and the first frame of GIF images as true-color terminal pixels.
- Browse using keyboard and mouse controls.
- Keep core state, HTML parsing, and the platform/network adapter in separate workspace crates.

Nyx does **not** currently connect to Tor, run JavaScript, or provide a Chromium-like browser engine. The `[Tor: DISCONNECTED]` label is an indicator, not an active Tor connection.

## Requirements

- Rust stable toolchain (edition 2021).
- Windows 11 or Linux for the currently supported development targets.
- A terminal with 24-bit color for image display.
- A running SearXNG instance with JSON output enabled for search.

## Build and run

```powershell
cargo test --workspace
cargo run -p nyx-tui
```

The first launch prompts you to create a passphrase for Nyx's session vault. Later launches prompt you to unlock it. Keep the passphrase safe: the vault is encrypted and cannot be recovered without it.

The vault location is printed when Nyx exits. Treat the vault file as sensitive even though its contents are encrypted.

## Search with SearXNG

Nyx sends search requests to `http://127.0.0.1:8080/search` by default. Run SearXNG locally and enable the JSON format in its `settings.yml`:

```yaml
server:
  secret_key: "replace-with-a-long-random-secret"
  limiter: false

search:
  formats:
    - html
    - json
```

Bind a local instance to loopback rather than exposing it publicly. For example, after saving the configuration as `settings.yml` in the current directory:

```powershell
docker run -d --name nyx-searxng --restart unless-stopped `
  -p 127.0.0.1:8080:8080 `
  -v "${PWD}/settings.yml:/etc/searxng/settings.yml:ro" `
  -e "BASE_URL=http://localhost:8080/" `
  searxng/searxng:latest
```

On Linux, use the same Docker options with shell line continuations (`\`) instead of PowerShell backticks.

To use another SearXNG endpoint, set `NYX_SEARXNG_URL` before starting Nyx:

```powershell
$env:NYX_SEARXNG_URL = "http://localhost:8080/search"
cargo run -p nyx-tui
```

The endpoint must accept `format=json` and `q=<query>` parameters. Nyx reports a clear search error if the service is unreachable or returns invalid JSON.

## Keyboard controls

| Key | Action |
| --- | --- |
| `Enter` | Submit a search or open the selected link |
| `Ctrl+L` | Return to the search/home screen |
| `Tab` / `Shift+Tab` | Cycle through links or form fields |
| `1`–`9` | Select a visible result or article link |
| `PageUp` / `PageDown` | Change result pages or scroll |
| `i` | Open the image viewer on an article page |
| `Left` / `Right` | Change images while the image viewer is open |
| `f` | Open a supported form on an article page |
| `Esc` | Close a modal or go back |
| `F1` | Toggle help |
| `q` | Quit |
| `Ctrl+C` | Quit immediately |

Mouse clicks select results and article link references; the wheel scrolls the page.

## Workspace

```text
crates/
  nyx-core/  Application state, HTML parsing, URL handling, and search parsing
  nyx-pal/   Network/system adapter traits, HTTP client, and encrypted session vault
  nyx-tui/   Ratatui/Crossterm terminal application
```

## Limitations

- HTML forms are limited to standard GET/POST controls; forms that depend on JavaScript, dynamic tokens, CAPTCHA, or complex OAuth may not work.
- Modern services such as Google sign-in and Gemini web commonly require JavaScript and browser verification, so they are not expected to work in Nyx.
- Image support covers common `img` elements and lazy-loaded `data-src` URLs. It does not execute page scripts or support every image format or terminal.
- A local SearXNG service improves control over search infrastructure, but its upstream search engines may still rate-limit or block requests.
- Axon OS support is a future goal. The current platform adapter depends on operating-system networking and is not yet ported to Axon.

## Development

Run formatting, tests, and lint checks before submitting a change:

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution guidelines and [SECURITY.md](SECURITY.md) for security reporting.

## License

Nyx is licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
