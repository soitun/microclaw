# OpenClaw Weixin

MicroClaw now supports OpenClaw Weixin in two modes:

1. Native Rust mode
2. Bridge mode

Native mode covers the core text flow directly in Rust:

- QR login
- persisted bot credentials
- long polling via `getupdates`
- inbound webhook compatibility
- text replies via `sendmessage`
- persisted `context_token` cache
- persisted `get_updates_buf`

Bridge mode remains available for:

- existing `@tencent-weixin/openclaw-weixin` deployments
- custom Node sidecars
- attachment delivery while native mode is still text-only

## Mode Selection

`channels.openclaw-weixin.mode` supports:

- `native`: always use native Rust login + polling + text send
- `bridge`: disable native polling and require `send_command`
- `auto`: prefer native when credentials exist or when `send_command` is empty; otherwise use bridge

Per-account overrides are also supported through `channels.openclaw-weixin.accounts.<id>.mode`.

## Native Config

Single-account example:

```yaml
channels:
  openclaw-weixin:
    enabled: true
    mode: native
    base_url: https://ilinkai.weixin.qq.com
    allowed_user_ids: "alice@im.wechat,bob@im.wechat"
```

Multi-account example:

```yaml
channels:
  openclaw-weixin:
    enabled: true
    mode: auto
    default_account: main
    accounts:
      main:
        mode: native
      ops:
        mode: bridge
        send_command: node /opt/weixin-bridge/send-ops.mjs
        webhook_token: replace-me-ops
```

## Native CLI

Login and persist credentials:

```sh
microclaw weixin login
microclaw weixin login --account ops
microclaw weixin login --account ops --base-url https://ilinkai.weixin.qq.com
```

Inspect local state:

```sh
microclaw weixin status
microclaw weixin status --account ops
```

Remove local credentials and sync cursor:

```sh
microclaw weixin logout
microclaw weixin logout --account ops
```

Native credentials are stored under:

- `<data_dir>/openclaw-weixin/accounts/<account>.json`
- `<data_dir>/openclaw-weixin/sync/<account>.txt`

## Native Behavior And Limits

- Native polling starts automatically on `microclaw start` when mode resolves to native and local credentials are present.
- Inbound messages may arrive from long polling or from a compatible webhook; both paths share the same normalization and agent loop.
- Replying requires a previously seen `context_token`, so proactive sends to a never-seen user are not possible yet.
- Native outbound delivery is currently text-only.
- If you need attachments today, keep `send_command` configured and use bridge mode for that account.

## Bridge Config

Bridge mode is still the right option when you want to keep `@tencent-weixin/openclaw-weixin` or any custom sidecar in charge of login and outbound delivery.

Single-account example:

```yaml
channels:
  openclaw-weixin:
    enabled: true
    mode: bridge
    webhook_path: /openclaw-weixin/messages
    webhook_token: replace-me
    send_command: node /opt/weixin-bridge/send.mjs
    allowed_user_ids: "alice@im.wechat,bob@im.wechat"
```

Multi-account example:

```yaml
channels:
  openclaw-weixin:
    enabled: true
    mode: bridge
    default_account: main
    accounts:
      main:
        send_command: node /opt/weixin-bridge/send-main.mjs
      ops:
        send_command: node /opt/weixin-bridge/send-ops.mjs
        webhook_token: replace-me-ops
```

## Inbound Webhook

Send `POST` requests to the configured `webhook_path`.

Headers:

- `Content-Type: application/json`
- `x-openclaw-weixin-webhook-token: <token>` when `webhook_token` is configured
- `Authorization: Bearer <token>` is also accepted as a fallback

Body:

```json
{
  "account_id": "main",
  "from_user_id": "alice@im.wechat",
  "text": "hello",
  "message_id": "wx-msg-123",
  "timestamp_ms": 1740000000000,
  "context_token": "ctx-123"
}
```

Fields:

- `account_id`: optional for single-account mode; selects `channels.openclaw-weixin.accounts.<id>`.
- `from_user_id`: required Weixin user id.
- `text`: required plain-text content forwarded into the agent loop.
- `message_id`: optional dedupe key.
- `timestamp_ms` or `timestamp`: optional startup-guard timestamp.
- `context_token`: strongly recommended and required for later replies to the same user.

MicroClaw also accepts a more upstream-like nested shape:

```json
{
  "account_id": "main",
  "message": {
    "from_user_id": "alice@im.wechat",
    "message_id": 42,
    "create_time_ms": 1740000000000,
    "context_token": "ctx-123",
    "item_list": [
      { "type": 1, "text_item": { "text": "hello" } }
    ]
  }
}
```

For `item_list`, MicroClaw currently normalizes:

- text -> plain text
- voice with transcript -> transcript text
- image -> `[image]`
- file -> `[file]` or `[file: <name>]`
- video -> `[video]`

## Bridge Outbound Command Contract

MicroClaw executes `send_command` with `sh -lc`.

Environment variables:

- `MICROCLAW_WEIXIN_ACTION`: `send_text` or `send_attachment`
- `MICROCLAW_WEIXIN_CHANNEL_NAME`: resolved MicroClaw channel name
- `MICROCLAW_WEIXIN_ACCOUNT_ID`: selected account id, or empty in single-account mode
- `MICROCLAW_WEIXIN_TARGET`: Weixin user id
- `MICROCLAW_WEIXIN_TEXT`: outbound text
- `MICROCLAW_WEIXIN_CONTEXT_TOKEN`: cached context token for that user
- `MICROCLAW_WEIXIN_ATTACHMENT_PATH`: attachment path for `send_attachment`
- `MICROCLAW_WEIXIN_ATTACHMENT_CAPTION`: optional attachment caption
- `MICROCLAW_WEIXIN_PAYLOAD`: JSON bundle containing the same fields

Example payload:

```json
{
  "action": "send_text",
  "channel_name": "openclaw-weixin",
  "account_id": "main",
  "target": "alice@im.wechat",
  "text": "hello",
  "context_token": "ctx-123",
  "attachment_path": "",
  "attachment_caption": ""
}
```

If you want a minimal starting point, use:

```yaml
send_command: WEIXIN_BRIDGE_URL=http://127.0.0.1:8788/send WEIXIN_BRIDGE_TOKEN=replace-me node examples/openclaw-weixin/send-command-http-forward.mjs
```

That helper simply forwards `MICROCLAW_WEIXIN_PAYLOAD` to your local sidecar over HTTP.

## Context Token Behavior

Weixin replies require a `context_token`. MicroClaw caches the latest token per `channel + user`.

Implications:

- A user must send at least one inbound message before MicroClaw can reply.
- Scheduled or proactive delivery to a never-seen Weixin user will fail until MicroClaw has seen one inbound message carrying a `context_token`.

## Using `@tencent-weixin/openclaw-weixin`

The npm package can still provide the Node-side login and long-polling flow. In that setup, the bridge only needs to translate:

- package inbound updates -> MicroClaw webhook payload above
- MicroClaw `send_command` env payload -> package send-message logic

Starter files:

- `examples/openclaw-weixin/README.md`
- `examples/openclaw-weixin/send-command-http-forward.mjs`

The package metadata and protocol description live at:

- npm: `https://www.npmjs.com/package/@tencent-weixin/openclaw-weixin`
- registry metadata: `https://registry.npmjs.org/@tencent-weixin/openclaw-weixin`
