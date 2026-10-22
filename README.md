# OG-Host

Headless, web-managed server OS for the homelab: our own Rust container engine
(Docker/OCI compatible), occasional VMs, and a resource brain that shares memory and
GPUs where they're needed. Managed through OmegaEnv, rewritten in Rust as its web UI.

Status: in development — engine, Docker API, builds, GPUs (CDI), resource brain and the web
UI work on a test VM. See [docs/DESIGN.md](docs/DESIGN.md).

Web UI: run `og-hostd serve --listen 127.0.0.1:8411`, create an account with
`og-hostd passwd admin`, then open the address (behind `tailscale serve` for HTTPS).
