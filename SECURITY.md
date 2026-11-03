# Security policy

Nyx is experimental software and is not currently suitable for handling high-risk or highly sensitive browsing.

## Reporting a vulnerability

Please do not publish exploitable details in a public issue. Use GitHub's private vulnerability reporting or security advisory feature for this repository if it is available. If private reporting is unavailable, contact the repository maintainer privately through GitHub and request a secure reporting channel.

Include the affected version or commit, operating system, impact, and concise reproduction steps. Do not include real passwords, cookies, session vaults, API keys, or other private data.

## Scope and security notes

- Treat the session vault file and its passphrase as sensitive. The vault is encrypted, but anyone with the passphrase and file can unlock stored session data.
- Nyx sends network requests to the URLs selected by the user and to the configured SearXNG endpoint.
- Nyx does not execute JavaScript. This reduces some browser attack surface but does not make arbitrary web content safe.
- Do not expose a local SearXNG development instance to an untrusted network without separately securing it.
- The `[Tor: DISCONNECTED]` status means traffic is not routed through Tor.

Security reports are reviewed on a best-effort basis; no response-time guarantee is currently provided.
