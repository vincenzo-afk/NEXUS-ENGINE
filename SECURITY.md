# Security Policy

## Supported code

Security fixes are evaluated against the current `main` branch. The repository does not currently publish versioned maintenance branches or make support-time commitments.

## Reporting a vulnerability

Please use GitHub’s **Report a vulnerability** flow on the repository’s Security tab for private disclosure. Include a clear description, the affected command or HTTP route, a minimal reproduction, impact, and any suggested mitigation. Do not post vulnerabilities, API keys, private index contents, file paths, or proof-of-concept exploit details in public issues.

The project’s public issue forms remain appropriate for non-sensitive bugs and feature requests. For search-service deployments, keep the native HTTP server on a trusted network boundary, protect any configured API keys, and expose `/open` or `/view` only when their local-file semantics are appropriate for the host environment.

## Security-relevant behavior

NEXUS includes bounded search requests, rate limiting, optional API authentication, configurable CORS, crawl constraints, and a browser-target build. These controls reduce particular risks but do not eliminate the need to place a public deployment behind appropriate authentication, TLS termination, request limits, and operational monitoring.
