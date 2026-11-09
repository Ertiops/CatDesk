# Tailscale Funnel transport (experimental fork feature)

This fork adds a **Tailscale Funnel** transport without requiring ngrok, a
purchased domain, or a VPS. CatDesk's MCP router and workspace workers run
locally on your machine. The remote client reaches the router through a
public `https://<node>.<tailnet>.ts.net/<secret>/mcp` address.

## Prerequisites

- A signed-in **open-source `tailscaled`** instance with the `tailscale` CLI
  accessible to the user running CatDesk. On macOS, the App Store/Standalone
  GUI variants must **not** be used for Funnel; they may report `Funnel on`
  while failing external TLS. Keep the GUI client untouched and run a separate
  userspace `tailscaled` instance on a private Unix socket instead.
- On macOS set the absolute `CATDESK_TAILSCALE_SOCKET` to that isolated Unix
  socket. `CATDESK_TAILSCALE_BIN` may also select an approved CLI executable;
  otherwise the Homebrew executable is detected automatically. Do not point
  the binary setting at a shell script.
- MagicDNS and HTTPS enabled for your tailnet, with Funnel authorized for
  this specific node in the Tailscale admin console. Tailscale may require
  interactive account authorization at initial setup.
- Tailscale Funnel availability under your plan and local Tailscale edition.
  Check the current Tailscale restrictions before relying on it.
- Port 3200 must be free: never run the forked router alongside the installed
  ngrok-based CatDesk router or another service on that port.

No additional ngrok account/token and no new host port forwarding are needed.
Do **not** replace the existing live CatDesk binary during initial tests.

## macOS: isolated userspace Tailscale (without touching the GUI VPN)

Use the Homebrew **formula** with no global symlinks/services, then run the
CLI-only daemon as the signed-in user. This provides a separate Tailscale node,
so standard public HTTPS port 443 is available without changing Tailscale Serve
routes on the existing Mac node. No TUN device, additional VPN or VPS is used.

```bash
brew install --formula --skip-link tailscale
umask 077
mkdir -p "$HOME/.local/run/catdesk-funnel" "$HOME/.local/state/catdesk-funnel"
chmod 700 "$HOME/.local/run/catdesk-funnel" "$HOME/.local/state/catdesk-funnel"
SOCKET="$HOME/.local/run/catdesk-funnel/daemon.sock"

# Run in a separate Terminal or a dedicated user LaunchAgent.
/opt/homebrew/opt/tailscale/bin/tailscaled \
  --tun=userspace-networking \
  --socket="$SOCKET" \
  --statedir="$HOME/.local/state/catdesk-funnel" \
  --port=0

# In another Terminal; login may require interactive approval.
/opt/homebrew/opt/tailscale/bin/tailscale --socket="$SOCKET" up \
  --hostname=catdesk-mcp --accept-dns=false --accept-routes=false --ssh=false

# For the CatDesk process AND its background router subprocess:
export CATDESK_TAILSCALE_SOCKET="$SOCKET"
export CATDESK_TAILSCALE_BIN=/opt/homebrew/opt/tailscale/bin/tailscale
```

Do not use `sudo brew services start tailscale`: that launches a different
system-wide daemon and can interfere with an existing VPN client. The daemon's
socket may have mode `0666` by design, so the **parent directory must stay
`0700`**. Protect the state directory (node identity and credentials) as well.
Configure a user LaunchAgent if the Tailscale node must survive logouts and
restarts, but do not automatically enable Funnel for unrelated services.

## Choose the transport

In CatDesk's initial **Settings**, select **Tunnel provider (Enter to
toggle)**, set `tailscale`, leave Settings, and then select a control mode.
This option is saved in `~/.catdesk/config.toml`:

```toml
tunnelProvider = "tailscale"
```

The default is `ngrok`, maintaining compatibility with existing CatDesk
configurations. The ngrok token and domain, if already configured, are left
unchanged but are unused in Tailscale mode. Do not store Tailscale credentials
in this file.

The selected provider is **not hot-swappable**: stop all CatDesk workspaces
before switching and restart the router. Settings block the switch when
other clients are live or the local server has started.

## What CatDesk executes

After choosing Tailscale, CatDesk (on macOS via the configured private
`--socket=<path>` CLI flag):

1. Calls `tailscale status --json` to confirm the local daemon reports a
   Running state and returns a valid MagicDNS `*.ts.net` hostname.
2. Reads `tailscale funnel status --json` and chooses an unused supported
   HTTPS port, preferring 443, then 10000, then 8443. An existing port is
   reused only if it already contains exactly this CatDesk Funnel mapping;
   foreign Serve/Funnel routes are never overwritten or made public.
3. Runs `tailscale funnel --bg --https=<public-port> http://127.0.0.1:3200`.
4. Computes the MCP URL using the existing random secret path. If the public
   port is not 443, the URL includes `:<public-port>`.
5. Exposes the same MCP tools and multi-workspace routing as ngrok.

A successful CLI invocation means the forwarding rule was **configured**.
It does **not** prove that ChatGPT can connect from the public internet.
Tailscale Funnel is public; tailnet-only Tailscale Serve is **not** a
substitute for a ChatGPT custom connector.

## Verification before cutover

- Verify the local router health endpoint at
  `http://127.0.0.1:3200/<slug>`; it must report
  `tunnelProvider=tailscale`, `tunnelRunning=true`, and
  `router=true`. Keep the slug private.
- Inspect `tailscale --socket="$CATDESK_TAILSCALE_SOCKET" funnel status`
  on the **isolated Tailscale node**, not on the GUI client. Ensure
  it forwards to the correct local port and did not overwrite a different
  existing Serve/Funnel rule. Nonstandard public ports (10000/8443) may not
  be supported by all remote MCP clients; verify the exact endpoint in
  ChatGPT before considering cutover.
- Connect a **test** ChatGPT custom MCP connector to the new full HTTPS MCP
  URL. First verify `tools/list`, then a benign `catdesk_instruction`
  call. Test read-only tools and relevant multi-workspace routing.
- Confirm externally that clients without the secret slug cannot invoke
  tools, and rotate the old MCP slug during cutover if it was exposed.

Build the fork with `cargo build --release --locked` and test the resulting
`target/release/catdesk`; `npm install -g catdesk` installs the **upstream**
implementation, not this fork. Only after the checks above, arrange a controlled
replacement of the production CatDesk binary and stop the old ngrok tunnel. Do not switch production
CatDesk, Teplo, any existing Tailscale gateway, or other running services
as part of development.

## Security and lifecycle

**Warning:** Funnel exposes a public internet endpoint capable of causing
local file access and command execution. A secret URL path is only a bearer
secret, not a replacement for strong authentication and scoped permissions.
Avoid transmitting the full URL in tickets, logs, screenshots or chat.
Use CatDesk's workspace scope and read-only tool permissions until verified;
consider stronger MCP authentication before continuous production exposure.

Tailscale's background Funnel configuration can persist independently of the
CatDesk process. **Do not** run `tailscale funnel reset` on a shared Tailscale
instance: it may remove unrelated mappings. Stop/reconfigure only the
specific mapping after inspecting its status. Avoid switching a host already
serving Teplo or other applications without an isolation plan.

Funnel's bandwidth and beta/service restrictions may change. Review the
current rules: https://tailscale.com/docs/features/tailscale-funnel and
https://tailscale.com/docs/reference/tailscale-cli/funnel

For rollback: stop all CatDesk workers, restore the selected provider to
`ngrok`, restart the previous known-good build, and verify the existing
ngrok connector. Do not erase existing config or router registry.
