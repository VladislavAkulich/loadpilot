# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| latest  | ✓         |

Only the latest released version receives security fixes.
Older versions are not backported.

## Reporting a Vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Report vulnerabilities by opening a
[GitHub Security Advisory](https://github.com/VladislavAkulich/loadpilot/security/advisories/new)
(requires a GitHub account).

Include:
- Description of the vulnerability and potential impact
- Steps to reproduce or a proof-of-concept
- Affected versions and components (coordinator, agent, CLI, PyO3 bridge)
- Any suggested fix if you have one

You will receive an acknowledgement within **5 business days** and a resolution
timeline within **14 days**.

## Scope

LoadPilot is a **load-testing tool** intended to run inside a trusted network
against servers you own or have explicit permission to test.

### In scope

- Vulnerabilities in the coordinator or agent binaries that could affect the
  machine running the tool (e.g. remote code execution via a crafted scenario
  file, path traversal, unsafe deserialization)
- Dependency vulnerabilities with a realistic attack vector in LoadPilot's
  usage model
- Secrets leaking through log output or metric exports

### Out of scope

- Using LoadPilot to attack third-party services without authorisation — this
  is a misuse of the tool, not a vulnerability in it
- Issues in the target server under test
- Denial-of-service against the machine running LoadPilot itself (it is a
  load generator by design)
- Vulnerabilities in dependencies with no realistic attack path

## Security Considerations for Users

- **Target authorisation** — only run load tests against hosts you own or have
  written permission to test.
- **NATS authentication** — always set `NATS_TOKEN` (or `--nats-token`) when
  exposing the NATS broker to a network; the embedded broker binds to
  `127.0.0.1` by default and should not be exposed to untrusted networks.
- **Coordinator HTTP API** — serve mode (`--serve`) listens on `0.0.0.0:8080`
  and accepts arbitrary scenario plans. Place it behind an authenticating
  reverse proxy or restrict access via network policy in Kubernetes.
- **Scenario files** — scenario `.py` files are executed as Python code.
  Treat them with the same trust level as any other code in your repository.
- **Sensitive headers** — HTTP headers specified in a scenario (e.g.
  `Authorization`) are transmitted to the target but are not stored or logged
  by LoadPilot.

## Dependency Auditing

Rust dependencies are audited with
[`cargo audit`](https://github.com/rustsec/rustsec):

```sh
cargo install cargo-audit
cd engine && cargo audit
```

Python dependencies are audited with
[`pip-audit`](https://github.com/pypa/pip-audit):

```sh
uv run pip-audit
```

Both checks run automatically in CI on each push.
