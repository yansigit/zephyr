# Security Policy

## Reporting a Vulnerability

Please do **not** report security vulnerabilities through public GitHub issues, discussions, or pull requests.

Report vulnerabilities privately through either of these channels:

- **GitHub Private Vulnerability Reporting:** [yansigit/zephyr/security/advisories/new](https://github.com/yansigit/zephyr/security/advisories/new)
- **Email:** [contact@sbyoon.com](mailto:contact@sbyoon.com)

Include a clear description of the issue, affected component(s), reproduction steps or a proof of concept, and any suggested remediation.

## Supported Versions

Security fixes are provided for:

| Version | Supported |
|---|---|
| Latest release | Yes |
| `main` | Yes |
| Older releases | No |

## Scope

In scope:

- The `zephyr` host CLI and container daemon (`crates/zephyr`)
- The `zephyr-policy` extension (`crates/zephyr-policy`) and `zephyr-cursor` provider bridge (`crates/zephyr-cursor`)
- Approval gates, private fact checks, memory approval enforcement, the vault and token broker (`zsur_` surrogate handling and scrubbing), the egress network floor, and web app authentication/authorization
- Container packaging and installation scripts under `deploy/`

Out of scope:

- Vulnerabilities in upstream model providers, third-party chat platforms, or dependencies unless exploitable through Zephyr itself
- Threat models that assume root or physical access to the host machine, or documented limitations noted in [README.md](README.md) and [DESIGN.md](DESIGN.md)
