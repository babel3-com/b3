# Babel3 Licensing

## Overview

Babel3 is a split-license project. The open-source components in this repository are licensed under the **Apache License, Version 2.0**. The server (not included in this repository) is proprietary.

> **License identifier:** `Apache-2.0`

## What's in This Repository (Apache 2.0)

| Component | Directory | Description |
|-----------|-----------|-------------|
| CLI Daemon | `crates/b3-cli/` | Rust binary — PTY, voice pipeline, MCP servers, mesh networking |
| Common Types | `crates/b3-common/` | Shared types, backoff, health checks, limits, TTS types |
| Browser UI | `browser/` | JavaScript/CSS — terminal, voice, LED, file browser |
| GPU Worker | `gpu-worker/` | Docker container — Chatterbox TTS, voice cloning, embeddings |
| Install Script | `install.sh` | Platform-detecting installer with SHA256 verification |
| Documentation | `docs/` | Architecture, guides, API reference |

### Default Rule

Any file in this repository not explicitly listed below as excluded follows the root `LICENSE` file (Apache-2.0).

## What's NOT in This Repository (Proprietary)

The following components are proprietary and are **not included** in this repository. They are hosted at babel3.com.

| Component | Description |
|-----------|-------------|
| **Server** | API server — auth, routing, billing, marketplace, hive clearinghouse |
| **Closed browser UI** | Drawer, credits, monetization UI components |
| **Server templates** | HTML templates for landing pages, hosted sessions |
| **CPU host orchestrator** | Hosted session container management |
| **Ollama serverless proxy** | RunPod serverless Ollama bridge |
| **Server runtime** | Migrations, nginx config, deploy artifacts |
| **Deploy scripts** | Production deployment automation |
| **WireGuard relay config** | Relay server configuration |
| **Landing page** | babel3.com marketing site |
| **Internal plugins** | Development tooling with internal infrastructure paths |

**Why this transparency matters:** You should know exactly what's open and what isn't. We don't hide the boundary — we document it.

## Why Apache 2.0

1. **Zero friction.** Fork authors, GPU operators, and enterprise customers can use, modify, and build on the code without any copyleft obligations. The marketplace grows faster when the license doesn't get in the way.
2. **Patent grant.** Apache 2.0 Section 3 gives every user and contributor an explicit, irrevocable patent license. Nobody can contribute code then sue you for patent infringement on that contribution. If anyone sues a contributor for patents related to the project, their patent license terminates automatically (defensive shield).
3. **Trademark protection.** Apache 2.0 Section 6 explicitly says the license does not grant permission to use the "Babel3" name or trademarks. Fork authors get the code but cannot use our brand.
4. **Built-in contribution terms.** Apache 2.0 Section 5 automatically licenses contributions under the same terms. No separate CLA required for basic contributions.
5. **Enterprise-friendly.** No enterprise legal team has ever blocked Apache 2.0. It's the industry standard for commercial open source (Kubernetes, Android, Swift, TensorFlow).

**Why not GPL?** We considered it seriously (see the LICENSE-DECISION-BRIEF for the full analysis). The marketplace model changed the equation: any friction that pushes developers to build outside the ecosystem costs more than copyleft protection gains. The server infrastructure — billing, marketplace, discovery — is the moat, not the license.

## What You Can Do

Apache 2.0 is a **permissive** license. You can:
- Use the code for any purpose, including commercial
- Modify the code and keep modifications private
- Bundle Babel3 code with proprietary code
- Build proprietary products on top of Babel3
- Charge for your products and services

You must:
- Include the Apache 2.0 license and copyright notice in distributions
- State any changes you made to the original files
- Not use the "Babel3" trademarks without permission

## For Fork Authors

Fork the repo. Build what you want. Your modifications can be open or proprietary — the license doesn't require sharing. If you participate in the Babel3 marketplace, you'll need a public GitHub fork (that's a marketplace policy, not a license requirement).

## For GPU Operators

Fork and modify the GPU worker freely. Add proprietary capabilities — custom models, specialized hardware optimizations, novel job types. The license imposes no restrictions on what you build. To participate in the marketplace, sign the GPU Operator Agreement (revenue share, billing compliance).

## Marketplace Requirements (Separate from License)

The following are **babel3.com platform policies**, not license terms. They govern access to the Babel3 marketplace and infrastructure, not the right to use the code:

- **GitHub fork requirement:** GPU operator registration and rogue browser serving require a public GitHub fork of this repository. This is a platform policy enforced by babel3.com.
- **GPU Operator Agreement:** GPU operators must sign a contractual agreement covering revenue share (~90% to operator, ~10% to babel3.com), acceptable use, and termination terms.
- **Rogue browser/GPU acknowledgment:** Users who choose third-party forks accept that babel3.com cannot verify what code the operator actually runs.

**You are free to use Apache 2.0-licensed Babel3 code without participating in the marketplace.** The license grants you that right independently of marketplace participation.

## Patent Notice

Certain innovations in this project are covered by pending patent applications filed by Babel17.ai LLC (US 64/010,742, US 64/010,891, US 64/011,207). Under Apache 2.0 Section 3, all users of this software receive an automatic, irrevocable, perpetual, worldwide patent license for the patent claims embodied in this code.

These patents protect against proprietary reimplementation by parties who do not use this open-source code. They impose no restrictions on users, contributors, or distributors of this software.

See [PATENTS](PATENTS) for the full list of covered technologies and what this means for you.

## Contributing

We use the Developer Certificate of Origin (DCO). Add `Signed-off-by` to your commits (`git commit -s`).

See [CONTRIBUTING.md](CONTRIBUTING.md) for details.
