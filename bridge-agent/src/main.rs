#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

//! `bridge-agent`: the in-guest agent for the Enlil inter-guest bridge.
//!
//! Speaks the bridge wire protocol from
//! [`enlil_devices::bridge::transport`] — 32-byte message frames addressed by
//! guest ID over the eight [`BridgeChannel`] queues — for clipboard,
//! drag-and-drop, and shared-filesystem exchange with other guests.
//!
//! In a production guest the agent binds to the guest's `VirtIO` bridge
//! device driver (any [`BridgeTransport`] implementation). This binary wires
//! the agent to a loopback transport so its protocol handling is exercisable
//! without a guest: `--self-test` performs end-to-end clipboard/DnD/shared-fs
//! round trips and exits non-zero on any mismatch.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bridge_agent::{
    AgentClient, SharedFsOp, decode_clipboard, decode_drag_payload, decode_sharedfs_op,
    loopback_client,
};
use clap::{Parser, Subcommand};
use enlil_devices::bridge::{
    clipboard::ClipboardContent,
    dragdrop::DragPayload,
    transport::{BridgeChannel, BridgeMessage},
};

/// In-guest agent for the Enlil inter-guest bridge.
#[derive(Debug, Parser)]
#[command(name = "bridge-agent", version, about)]
struct Cli {
    /// Numeric ID of this guest on the bridge.
    #[arg(long, default_value_t = 1)]
    guest_id: u16,

    /// Default destination guest ID for outgoing messages.
    #[arg(long, default_value_t = 2)]
    peer_id: u16,

    /// Run the protocol self-test against a loopback transport and exit.
    #[arg(long)]
    self_test: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Clipboard exchange with another guest.
    Clipboard {
        #[command(subcommand)]
        op: ClipboardOp,
    },
    /// Drag-and-drop exchange with another guest.
    Dnd {
        #[command(subcommand)]
        op: DndOp,
    },
    /// Shared-filesystem exchange with another guest.
    Sharedfs {
        #[command(subcommand)]
        op: SharedFsCmd,
    },
    /// Run the agent event loop, dispatching incoming bridge messages.
    Run {
        /// Stop after processing this many messages (0 = run until interrupted).
        #[arg(long, default_value_t = 0)]
        max_messages: u64,
    },
}

#[derive(Debug, Subcommand)]
enum ClipboardOp {
    /// Send clipboard content to a guest.
    Send {
        /// Text to place on the peer's clipboard.
        #[arg(long)]
        text: String,
        /// Destination guest ID (defaults to --peer-id).
        #[arg(long)]
        dst: Option<u16>,
    },
    /// Poll once for an incoming clipboard message and print it.
    Recv,
}

#[derive(Debug, Subcommand)]
enum DndOp {
    /// Send a drag-and-drop payload to a guest.
    Send {
        /// File URIs in the payload (repeatable).
        #[arg(long)]
        uri: Vec<String>,
        /// MIME types the payload offers (repeatable).
        #[arg(long)]
        mime: Vec<String>,
        /// Destination guest ID (defaults to --peer-id).
        #[arg(long)]
        dst: Option<u16>,
    },
}

#[derive(Debug, Subcommand)]
enum SharedFsCmd {
    /// Stage a local file into the shared area of a guest.
    Stage {
        /// Local file to stage.
        file: PathBuf,
        /// Destination guest ID (defaults to --peer-id).
        #[arg(long)]
        dst: Option<u16>,
    },
    /// Ask a guest for its shared-area listing.
    List {
        /// Destination guest ID (defaults to --peer-id).
        #[arg(long)]
        dst: Option<u16>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = loopback_client(cli.guest_id);

    if cli.self_test {
        return run_self_test(&client);
    }

    let Some(command) = cli.command else {
        // No subcommand: show help rather than silently doing nothing.
        use clap::CommandFactory as _;
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    match command {
        Command::Clipboard { op } => clipboard_cmd(&client, cli.peer_id, op),
        Command::Dnd { op } => dnd_cmd(&client, cli.peer_id, op),
        Command::Sharedfs { op } => sharedfs_cmd(&client, cli.peer_id, op),
        Command::Run { max_messages } => run_loop(&client, max_messages),
    }
}

fn clipboard_cmd(client: &AgentClient, peer_id: u16, op: ClipboardOp) -> Result<()> {
    match op {
        ClipboardOp::Send { text, dst } => {
            let dst = dst.unwrap_or(peer_id);
            let seq = client
                .send_clipboard_text(dst, &text)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "sent clipboard text to guest {dst} (seq {seq}, {} bytes)",
                text.len()
            );
        }
        ClipboardOp::Recv => match client.recv() {
            Some(msg) if msg.header.channel == BridgeChannel::Clipboard => {
                print_clipboard(&msg);
            }
            Some(msg) => {
                println!(
                    "next queued message is on {:?}, not clipboard; run `run` to dispatch it",
                    msg.header.channel
                );
            }
            None => println!("no bridge message pending"),
        },
    }
    Ok(())
}

fn dnd_cmd(client: &AgentClient, peer_id: u16, op: DndOp) -> Result<()> {
    match op {
        DndOp::Send { uri, mime, dst } => {
            let dst = dst.unwrap_or(peer_id);
            let payload = DragPayload::new(uri.clone(), mime.clone());
            let seq = client
                .send_drag_payload(dst, &payload)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "sent drag payload to guest {dst} (seq {seq}, {} uris, {} mime types)",
                uri.len(),
                mime.len()
            );
        }
    }
    Ok(())
}

fn sharedfs_cmd(client: &AgentClient, peer_id: u16, op: SharedFsCmd) -> Result<()> {
    match op {
        SharedFsCmd::Stage { file, dst } => {
            let dst = dst.unwrap_or(peer_id);
            let data =
                std::fs::read(&file).with_context(|| format!("cannot read {}", file.display()))?;
            let name = file
                .file_name()
                .and_then(|n| n.to_str())
                .with_context(|| format!("unusable file name: {}", file.display()))?
                .to_string();
            let bytes = data.len();
            let seq = client
                .send_sharedfs_op(
                    dst,
                    &SharedFsOp::Put {
                        name: name.clone(),
                        data,
                    },
                )
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("staged {name} ({bytes} bytes) to guest {dst} (seq {seq})");
        }
        SharedFsCmd::List { dst } => {
            let dst = dst.unwrap_or(peer_id);
            let seq = client
                .send_sharedfs_op(dst, &SharedFsOp::List)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("requested shared-area listing from guest {dst} (seq {seq})");
        }
    }
    Ok(())
}

/// Poll the transport and dispatch incoming bridge messages to stdout.
fn run_loop(client: &AgentClient, max_messages: u64) -> Result<()> {
    println!(
        "bridge-agent listening as guest {} (Ctrl-C to stop)",
        client.guest_id().id()
    );
    let mut processed: u64 = 0;
    loop {
        match client.recv() {
            Some(msg) => {
                dispatch(&msg);
                processed += 1;
                if max_messages > 0 && processed >= max_messages {
                    println!("processed {processed} message(s); stopping");
                    return Ok(());
                }
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

/// Render one incoming message for the operator.
fn dispatch(msg: &BridgeMessage) {
    let from = msg.header.src.id();
    match msg.header.channel {
        BridgeChannel::Clipboard => print_clipboard(msg),
        BridgeChannel::DragDrop => match decode_drag_payload(&msg.payload) {
            Ok(payload) => println!(
                "dnd from guest {from}: {} uris [{}], mime [{}]{}",
                payload.file_uris.len(),
                payload.file_uris.join(", "),
                payload.mime_types.join(", "),
                payload
                    .preview_thumbnail
                    .as_ref()
                    .map_or(String::new(), |t| format!(", {}-byte thumbnail", t.len())),
            ),
            Err(e) => println!("dnd from guest {from}: undecodable payload ({e})"),
        },
        BridgeChannel::SharedFs => match decode_sharedfs_op(&msg.payload) {
            Ok(SharedFsOp::Put { name, data }) => {
                println!(
                    "sharedfs put from guest {from}: {name} ({} bytes)",
                    data.len()
                );
            }
            Ok(SharedFsOp::Get { name }) => {
                println!("sharedfs get from guest {from}: {name}");
            }
            Ok(SharedFsOp::List) => {
                println!("sharedfs list request from guest {from}");
            }
            Ok(SharedFsOp::Delete { name }) => {
                println!("sharedfs delete from guest {from}: {name}");
            }
            Err(e) => println!("sharedfs from guest {from}: undecodable payload ({e})"),
        },
        channel => println!(
            "ignoring message on {channel:?} from guest {from} ({} bytes)",
            msg.payload.len()
        ),
    }
}

fn print_clipboard(msg: &BridgeMessage) {
    let from = msg.header.src.id();
    match decode_clipboard(&msg.payload) {
        Ok(ClipboardContent::Text(text)) => {
            println!("clipboard text from guest {from}: {text}");
        }
        Ok(ClipboardContent::Html(html)) => {
            println!("clipboard html from guest {from}: {html}");
        }
        Ok(ClipboardContent::FileRef(path)) => {
            println!("clipboard file-ref from guest {from}: {path}");
        }
        Ok(ClipboardContent::Image(data)) => {
            println!("clipboard image from guest {from}: {} bytes", data.len());
        }
        Ok(ClipboardContent::RichText(data)) => {
            println!(
                "clipboard rich-text from guest {from}: {} bytes",
                data.len()
            );
        }
        Err(e) => println!("clipboard from guest {from}: undecodable payload ({e})"),
    }
}

/// End-to-end protocol self-test over the loopback transport.
///
/// Exercises the exact send path the binary uses for real messages:
/// clipboard text + binary clipboard content, drag-and-drop, and a
/// shared-fs put, each framed as a [`BridgeMessage`] on its
/// [`BridgeChannel`] and decoded back. Any mismatch is a hard failure.
fn run_self_test(client: &AgentClient) -> Result<()> {
    let mut checks: u32 = 0;

    // 1. Clipboard text round trip (the minimum the task requires).
    let text = "bridge-agent self-test clipboard payload";
    let seq = client
        .send_clipboard_text(2, text)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let msg = client
        .recv()
        .context("self-test: no clipboard message received")?;
    if msg.header.channel != BridgeChannel::Clipboard {
        bail!(
            "self-test: expected clipboard channel, got {:?}",
            msg.header.channel
        );
    }
    if msg.header.seq != seq {
        bail!(
            "self-test: seq mismatch: sent {seq}, got {}",
            msg.header.seq
        );
    }
    match decode_clipboard(&msg.payload).map_err(|e| anyhow::anyhow!("{e}"))? {
        ClipboardContent::Text(got) if got == text => {}
        other => bail!("self-test: clipboard text mismatch: {other:?}"),
    }
    checks += 1;
    println!("ok 1 - clipboard text round trip (seq {seq})");

    // 2. Binary clipboard content round trip.
    let image = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A];
    client
        .send_clipboard(2, &ClipboardContent::Image(image.clone()))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let msg = client
        .recv()
        .context("self-test: no clipboard image message received")?;
    match decode_clipboard(&msg.payload).map_err(|e| anyhow::anyhow!("{e}"))? {
        ClipboardContent::Image(got) if got == image => {}
        other => bail!("self-test: clipboard image mismatch: {other:?}"),
    }
    checks += 1;
    println!("ok 2 - clipboard binary round trip");

    // 3. Drag-and-drop round trip.
    let payload = DragPayload::new(
        vec!["file:///self-test/drop.txt".to_string()],
        vec!["text/plain".to_string()],
    )
    .with_thumbnail(vec![1, 2, 3]);
    client
        .send_drag_payload(2, &payload)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let msg = client
        .recv()
        .context("self-test: no drag-and-drop message received")?;
    if msg.header.channel != BridgeChannel::DragDrop {
        bail!(
            "self-test: expected dragdrop channel, got {:?}",
            msg.header.channel
        );
    }
    let decoded = decode_drag_payload(&msg.payload).map_err(|e| anyhow::anyhow!("{e}"))?;
    if decoded.file_uris != payload.file_uris
        || decoded.mime_types != payload.mime_types
        || decoded.preview_thumbnail != Some(vec![1, 2, 3])
    {
        bail!("self-test: drag payload mismatch: {decoded:?}");
    }
    checks += 1;
    println!("ok 3 - drag-and-drop round trip");

    // 4. Shared-fs put round trip.
    let op = SharedFsOp::Put {
        name: "self-test.bin".to_string(),
        data: vec![9, 9, 9],
    };
    client
        .send_sharedfs_op(2, &op)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let msg = client
        .recv()
        .context("self-test: no shared-fs message received")?;
    if msg.header.channel != BridgeChannel::SharedFs {
        bail!(
            "self-test: expected sharedfs channel, got {:?}",
            msg.header.channel
        );
    }
    let decoded = decode_sharedfs_op(&msg.payload).map_err(|e| anyhow::anyhow!("{e}"))?;
    if decoded != op {
        bail!("self-test: shared-fs op mismatch: {decoded:?}");
    }
    checks += 1;
    println!("ok 4 - shared-fs put round trip");

    // 5. Malformed payloads are rejected, not misinterpreted.
    if decode_clipboard(&[]).is_ok() || decode_drag_payload(&[0]).is_ok() {
        bail!("self-test: malformed payload accepted");
    }
    checks += 1;
    println!("ok 5 - malformed payloads rejected");

    println!("bridge-agent self-test: PASS ({checks}/5 checks)");
    Ok(())
}
