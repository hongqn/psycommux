# psycommux

Drive [cmux](https://cmux.com) from your phone. psycommux is a small server you
run on the Mac next to cmux, and an installable web app (PWA) that looks and
works like the Claude app, with cmux's workspaces as the organizing layer.

The name comes from Gundam's psycommu, the system a Newtype uses to steer a
swarm of remote funnels: here, one phone steering many coding agents.

- **Drawer**: cmux's workspaces, each expandable to what it holds: Claude
  Code sessions (✳), terminals (›_), browsers (◍), Markdown documents (¶) and
  file previews (▤). A workspace shows a spinner while an agent works, "Needs
  your input" when one waits on you, and its unread count.
- **Claude Code sessions** open as a chat read from the session's transcript:
  your messages as bubbles, replies as Markdown, tool calls folded into steps
  with diffs and output, AskUserQuestion as tappable options, and permission
  prompts with 1/2/3/Esc. The composer sends a prompt; while Claude works its
  button stops it. A Terminal switch shows the raw screen.
- **Terminals** live and in color, with keys (Enter, Esc, Ctrl-C, Tab, ⇧Tab,
  arrows, Ctrl-D, PgUp/PgDn).
- **Browsers** as a live screenshot rendered at the device's size: tap to
  click, drag or wheel to scroll, type into the focused field, plus an address
  bar with back, forward and reload.
- **Markdown and file previews** rendered, following the file as it changes.
- **New session**: pick a folder, optionally describe a task; a new workspace
  starts `claude` there and opens its chat.
- **Dictation**: the composer's mic button types what you say at the caret,
  using the browser's speech recognition (Chrome sends the audio to Google,
  Safari to Apple). It starts in Chinese when Chinese is among the system
  languages; a button on the recording bar switches language, and ✕ restores
  the text from before. Review the text, then send. The button is hidden where
  the browser has no speech recognition; the keyboard's own dictation key
  works there too.
- **Notifications** in a list laid out like cmux's, with browser
  notifications; opening an item marks its notifications read.

One self-contained binary: the page, script and styles are compiled in.

## Install

```bash
cargo install --path .
```

## Run

Start it **from a terminal inside cmux**. cmux's default socket mode,
`cmuxOnly`, trusts processes started from cmux terminals, so no socket password
is needed:

```bash
psycommux
```

Keep it in the foreground of that terminal (or a child of it, such as an
agent's background task). `cmuxOnly` checks the process ancestry, so `nohup
... &`, `launchd` or a detached `screen` reparent psycommux away from cmux and
the socket answers `Access denied`; psycommux then exits with that message. To
run it detached, switch cmux's socket to `password` mode and export
`CMUX_SOCKET_PASSWORD`.

It prints a one-time login URL such as
`http://127.0.0.1:8787/login?token=...`. Opening it sets an HttpOnly cookie for
30 days. Set `PSYCOMMUX_TOKEN` to keep the same token across restarts.

| Option / env | Meaning |
| --- | --- |
| `--host`, `--port` | Bind address, default `127.0.0.1:8787`. |
| `--socket` / `CMUX_SOCKET_PATH` | cmux socket. Defaults to the one the surrounding cmux terminal exports. |
| `CMUX_SOCKET_PASSWORD` | Socket password, when cmux's socket mode is `password`. |
| `PSYCOMMUX_TOKEN` | Fixed access token instead of a random one. |
| `--public-url <url>` | The URL other devices use, printed as a QR code and allowed as an origin. Defaults to the `tailscale serve` HTTPS name that proxies to `--port`, or `http://<--host>:<port>` for a specific non-loopback `--host`. |
| `--tls-cert <pem>`, `--tls-key <pem>` | Serve HTTPS directly. The key may live in the cert file; without `--tls-key`, `<name>.key` next to `<name>.crt` is used, as `tailscale cert` writes them. With a certificate for a DNS name, the phone URL and QR code use that name. |
| `--allow-origin <url>` | Extra origin allowed to POST, e.g. when a proxy rewrites `Host`. |
| `--no-qr` | Print the phone URL without the QR code. |
| `--access-log` | Log every request. Off by default: only errors are logged, because a log line per poll would keep changing psycommux's own terminal, and a page watching that terminal would poll again at once, forever. |

## Use it from your phone

Do not bind to `0.0.0.0` on an open network: whoever has the token can type
into your terminals, which means running any command as you. Put an
authenticated, encrypted tunnel in front of it. With
[Tailscale](https://tailscale.com), either proxy it:

```bash
tailscale serve --bg --https=443 http://127.0.0.1:8787
psycommux            # finds the Serve name and prints a QR code for it
```

or serve HTTPS on the Mac's tailnet address with a Tailscale certificate
(enable HTTPS in the tailnet's DNS settings first):

```bash
tailscale cert mac.example.ts.net                          # writes .crt and .key
psycommux --host 100.x.y.z --tls-cert mac.example.ts.net.crt
```

Scan the QR code, then add the page to the Home Screen: it opens as a
standalone app, and on iOS (16.4+) that is also what browser notifications
need. Plain HTTP on the tailnet address works too, but browsers treat it as
insecure, so the page cannot show notifications.

Tailscale certificates come from Let's Encrypt, so the Mac's tailnet name
appears in public Certificate Transparency logs, and they expire after 90
days: rerun `tailscale cert`, then restart psycommux.

## How it works

```
phone ──HTTP/SSE──▶ psycommux ──unix socket──▶ cmux.app
                       ├─ one long-lived events.stream
                       ├─ one short connection per API call
                       └─ reads Claude Code transcripts and cmux's saved session
```

| HTTP | cmux |
| --- | --- |
| `GET /api/tree` | `system.tree`, `extension.sidebar.snapshot`, `chat.sessions.dump`, and cached v1 `sidebar_state` (status pills, log, progress) |
| `POST /api/screens {surfaces: {id: revision}}` | Waits until any listed terminal changes (`surface.wait_text_change`, or `surface.read_text` polled on older cmux), with colors from `terminal.replay` |
| `POST /api/send`, `/api/key`, `/api/peek` | `surface.send_text`, `surface.send_key`, `surface.read_text` |
| `POST /api/chat/{history,poll,send,interrupt} {session_id}` | The session's transcript, read backwards in pages and forward from a byte offset; `mobile.chat.send` / `mobile.chat.interrupt` |
| `POST /api/browser/{shot,act,release}` | `browser.screenshot`, `browser.viewport.set`, `browser.eval`, `browser.press`, `browser.navigate` |
| `POST /api/file {surface_id}` | The file a Markdown or file-preview panel shows |
| `POST /api/session/new {cwd, prompt?}` | `workspace.create` with `initial_input: "claude '<prompt>'\n"` |
| `POST /api/workspace/{select,create}` | `workspace.select`, `workspace.create` |
| `GET /api/notifications`, `POST /api/notifications/{read,dismiss,clear_all}` | `notification.*` |
| `GET /api/events` (SSE) | `events.stream`, with notification bodies looked up (the stream redacts them) |

API calls also accept `Authorization: Bearer <token>`:

```bash
curl -H "Authorization: Bearer $PSYCOMMUX_TOKEN" http://127.0.0.1:8787/api/tree
```

### Limits

- Terminals are redrawn from snapshots, not streamed byte by byte.
- A browser panel off the Mac's screen has no layout, so while you look at one
  psycommux lays it out at your phone's size with `browser.viewport.set`. The
  Mac shows that panel scaled until you leave, or 20 seconds after the phone
  stops looking. Taps are synthetic clicks through `browser.eval`, so pages
  that check for trusted input events ignore them.
- The socket does not say which file a Markdown panel shows, so psycommux
  reads `filePath` from cmux's saved session
  (`~/Library/Application Support/cmux/session-com.cmuxterm.app.json`). That is
  cmux's internal format; if it changes, those panels show "not available".
- Only Claude Code sessions open as chats; Codex sessions show their terminal.

## Security

- Every API route needs the token (cookie or bearer), compared in constant time.
- The cookie is `HttpOnly; SameSite=Lax` (`Secure` over HTTPS). Not
  `Strict`: an iOS Home Screen app launched cold opens the page as if from
  another site, and would ask for the token every time. State-changing
  requests must be JSON POSTs from the same origin, or carry no `Origin`, as
  from curl.
- The token never appears in the log.
- Transcript paths stay on the server; the page names sessions by the ids cmux
  reports.
- The browser address bar accepts only http and https URLs.
- `/api/send` skips the "don't type over an agent's prompt draft" check that
  `cmux send` does. Check the screen before you send.

## Development

```bash
cargo test
```

The tests run psycommux against a fake cmux socket, so they need neither cmux
nor a network. The TLS tests make a throwaway certificate with `openssl` and
call `curl`.

psycommux began as the `examples/cmux-web-remote` prototype in the cmux
repository, written in Python.
