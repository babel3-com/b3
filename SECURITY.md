# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in Babel3, please report it responsibly.

**Do NOT open a public GitHub issue for security vulnerabilities.**

Instead, email **security@babel3.com** with:

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if you have one)

## Response Timeline

- **Acknowledgment:** Within 48 hours
- **Initial assessment:** Within 5 business days
- **Fix timeline:** Depends on severity, but we aim for:
  - Critical: patch within 48 hours
  - High: patch within 7 days
  - Medium/Low: next release cycle

## What Qualifies

- Remote code execution
- Authentication bypass
- Privilege escalation
- Data exposure (terminal content, voice recordings, API keys)
- Cross-agent message interception
- Hive encryption weaknesses
- GPU worker input validation issues

## What Doesn't Qualify

- Denial of service via resource exhaustion (known limitation of self-hosted systems)
- Social engineering attacks
- Issues in dependencies (report upstream, but let us know)
- Anything requiring physical access to the user's machine

## Safe Harbor

We will not pursue legal action against security researchers who:
- Act in good faith
- Report vulnerabilities through the process above
- Do not access, modify, or delete other users' data
- Do not disclose publicly before we've had reasonable time to fix

Thank you for helping keep Babel3 secure.
