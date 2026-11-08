# Hatch

Browse an iPhone's files over USB. Let an AI agent do the same through MCP.

Hatch is a Linux desktop app. It uses Rust, [gpui-kit](https://gpui-kit.com), and the [idevice](https://crates.io/crates/idevice) crate.

## Run

You need `usbmuxd` and an iPhone connected by USB.

```sh
cargo run
```

Unlock the phone and tap Trust when it asks.

## Connect an agent

Hatch runs an MCP server at `http://127.0.0.1:7433/mcp` while the app is open.

```sh
claude mcp add --transport http hatch http://127.0.0.1:7433/mcp
```

Settings > Connect an agent has commands for other agents.

Agent writes are off by default. Turn them on in the sidebar.

## Scope

iOS gives USB access to the media partition (photos, videos, downloads) and to the Documents folder of apps that allow file sharing. It does not give access to the full filesystem.
