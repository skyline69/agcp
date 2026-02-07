# Security Policy

AGCP handles OAuth tokens and proxies API credentials between clients and Google Cloud Code. Security issues in this project can have real impact on users' credentials and API access.

## Reporting a Vulnerability

If you discover a security vulnerability, please report it through [GitHub's private security advisory feature](https://github.com/skyline69/agcp/security/advisories/new).

Please include:

- A description of the vulnerability
- Steps to reproduce or a proof of concept
- The potential impact

I will acknowledge receipt within 72 hours and aim to provide a fix or mitigation plan within 30 days. I request a **90-day disclosure window** before any public disclosure to allow time for a patch and coordinated release.

Please do **not** open a public GitHub issue for security vulnerabilities.

## What Counts as a Security Issue

- OAuth token leakage or exposure (in logs, error messages, responses, etc.)
- Authentication or authorization bypass
- Injection attacks (header injection, request smuggling, etc.)
- Credential exposure through proxy behavior
- Path traversal or unauthorized file access
- Denial of service through resource exhaustion in token/session handling

## What Does NOT Count

- The OAuth client ID and client secret embedded in the source code. These are **intentionally public**. AGCP uses Google's "installed application" OAuth flow, which is designed for native/CLI apps where the client secret cannot be kept confidential. This is expected behavior per Google's OAuth documentation.

## Supported Versions

Security fixes are applied to the latest release only. There is no backporting to older versions at this time.

## License

This project is licensed under the MIT License.
