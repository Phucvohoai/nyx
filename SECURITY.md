# Security policy

Nyx is experimental software and is not currently suitable for handling high-risk or highly sensitive browsing.

## Reporting a vulnerability

Please do not publish exploitable details in a public issue. Use GitHub's private vulnerability reporting or security advisory feature for this repository if it is available. If private reporting is unavailable, contact the repository maintainer privately through GitHub and request a secure reporting channel.

Include the affected version or commit, operating system, impact, and concise reproduction steps. Do not include real passwords, cookies, session vaults, API keys, or other private data.

## Scope and security notes

- Treat the session vault file and its passphrase as sensitive. The vault is encrypted, but anyone with the passphrase and file can unlock stored session data.
- Direct networking is the default. `Ctrl+T` enables a configured `socks5h` Tor proxy (default `socks5h://127.0.0.1:9050`) and verifies it with the Tor Project check endpoint before permitting requests; there is no direct fallback. Nyx does not install or start the Tor service.
- Requests to loopback addresses bypass the Tor proxy for local services such as SearXNG. A local SearXNG instance's own upstream requests are outside Nyx's Tor route.
- Tor mode uses a Firefox-like User-Agent and omits Chromium client hints, but Nyx does not reproduce Tor Browser's HTTP/TLS fingerprint or anonymity protections.
- Nyx does not execute JavaScript. This reduces some browser attack surface but does not make arbitrary web content safe.
- Do not expose a local SearXNG development instance to an untrusted network without separately securing it.
- The TUI distinguishes direct mode from a verified Tor route. Tor routing covers Nyx requests only; it does not proxy other applications or guarantee anonymity.

Security reports are reviewed on a best-effort basis; no response-time guarantee is currently provided.
