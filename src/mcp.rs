//! MCP server over stdio. It is a thin front for the daemon, so an agent and the
//! live window share one session with the phone.

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::daemon::{self, Daemon};

const PROTOCOL_VERSION: &str = "2025-06-18";

fn tools() -> Value {
    let point = |what: &str| json!({"type": "number", "description": what});
    json!([
        {
            "name": "screenshot",
            "description": "Capture the iPhone's screen once it has finished reacting to the last action and \
                stopped moving (or after 3 seconds, if it never does). Returns a JPEG and its pixel size. \
                All coordinates passed to other tools are pixels in this image.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "tap",
            "description": "Tap the iPhone's screen at a pixel position in the latest screenshot.",
            "inputSchema": {
                "type": "object",
                "properties": {"x": point("pixels from the left"), "y": point("pixels from the top")},
                "required": ["x", "y"]
            }
        },
        {
            "name": "swipe",
            "description": "Drag a finger between two pixel positions. Swipe up to scroll down. \
                Use a longer duration_ms (800+) for a slow drag, shorter (150) for a flick.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x1": point("start, pixels from the left"),
                    "y1": point("start, pixels from the top"),
                    "x2": point("end, pixels from the left"),
                    "y2": point("end, pixels from the top"),
                    "duration_ms": {"type": "integer", "description": "default 400"}
                },
                "required": ["x1", "y1", "x2", "y2"]
            }
        },
        {
            "name": "home",
            "description": "Go to the iPhone's home screen.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "app_switcher",
            "description": "Open the app switcher to see and switch between recent apps.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "pinch",
            "description": "Two-finger pinch centred on a pixel position. scale above 1 zooms in, below 1 zooms out.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": point("centre, pixels from the left"),
                    "y": point("centre, pixels from the top"),
                    "scale": {"type": "number", "description": "e.g. 2 to zoom in, 0.5 to zoom out"}
                },
                "required": ["x", "y", "scale"]
            }
        },
        {
            "name": "list_elements",
            "description": "List what is on the iPhone's screen as accessibility elements: each \
                one's label (as VoiceOver would read it, including values such as 'Wi-Fi, On') and \
                the pixel position of its centre in the screenshot. More reliable than reading a \
                screenshot for finding buttons, switches and their states. Takes a few seconds and \
                briefly flashes a highlight across the phone's screen.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "tap_element",
            "description": "Tap the on-screen element whose accessibility label matches (case-insensitive; \
                a label that starts with the text is preferred, then one that contains it).",
            "inputSchema": {
                "type": "object",
                "properties": {"label": {"type": "string"}},
                "required": ["label"]
            }
        },
        {
            "name": "list_apps",
            "description": "List the apps installed on the iPhone, with their bundle identifiers.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "launch_app",
            "description": "Open an app directly by name or bundle identifier, without finding its icon.",
            "inputSchema": {
                "type": "object",
                "properties": {"app": {"type": "string", "description": "e.g. 'Settings' or 'com.apple.Preferences'"}},
                "required": ["app"]
            }
        },
        {
            "name": "start_recording",
            "description": "Start recording the iPhone's screen and sound to an MP4 on the PC.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "stop_recording",
            "description": "Stop the recording and return the path of the finished MP4.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "set_location",
            "description": "Make the iPhone report a simulated GPS location until clear_location is called.",
            "inputSchema": {
                "type": "object",
                "properties": {"latitude": point("degrees, -90 to 90"), "longitude": point("degrees, -180 to 180")},
                "required": ["latitude", "longitude"]
            }
        },
        {
            "name": "clear_location",
            "description": "Stop simulating a location; the iPhone reports where it really is again.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "app_performance",
            "description": "What the app in front is using right now: CPU (percent of one core), memory and frames per second.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "back",
            "description": "Go back one screen (swipes in from the left edge).",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "spotlight",
            "description": "Open Spotlight search; follow with type_text to search for an app or setting.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "control_center",
            "description": "Open Control Center.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "notifications",
            "description": "Open Notification Centre.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "press_button",
            "description": "Press a hardware button.",
            "inputSchema": {
                "type": "object",
                "properties": {"button": {
                    "type": "string",
                    "enum": ["lock", "volume-up", "volume-down", "siri"]
                }},
                "required": ["button"]
            }
        },
        {
            "name": "type_text",
            "description": "Type text into the focused field as a hardware keyboard would (US layout, ASCII only).",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }
        },
        {
            "name": "press_key",
            "description": "Press one keyboard key.",
            "inputSchema": {
                "type": "object",
                "properties": {"key": {
                    "type": "string",
                    "enum": ["return", "backspace", "delete", "tab", "space", "esc", "up", "down", "left", "right"]
                }},
                "required": ["key"]
            }
        }
    ])
}

async fn frame_size(daemon: &Daemon) -> Result<(f64, f64)> {
    let (_, body) = daemon.request("GET", "/status", b"").await?;
    let status: Value = serde_json::from_slice(&body)?;
    match (status["width"].as_f64(), status["height"].as_f64()) {
        (Some(w), Some(h)) => Ok((w, h)),
        _ => bail!(
            "no picture from the phone yet: {}",
            status["problem"].as_str().unwrap_or("waiting for the first frame — is it unlocked?")
        ),
    }
}

/// Sends an action and returns the body of a successful reply.
async fn post(daemon: &Daemon, action: Value) -> Result<Vec<u8>> {
    let (status, body) = daemon.request("POST", "/api", action.to_string().as_bytes()).await?;
    if status != 200 {
        bail!("{}", String::from_utf8_lossy(&body));
    }
    Ok(body)
}

async fn elements(daemon: &Daemon) -> Result<Vec<Value>> {
    Ok(serde_json::from_slice(&post(daemon, json!({"t": "elements"})).await?)?)
}

/// The middle of an element's box, in screenshot pixels, if it is on screen.
fn centre(element: &Value) -> Option<(u64, u64)> {
    let rect = element.get("rect")?;
    let part = |key: &str| rect[key].as_u64();
    Some((part("x")? + part("width")? / 2, part("y")? + part("height")? / 2))
}

async fn act(daemon: &Daemon, action: Value) -> Result<Vec<Value>> {
    let (status, body) = daemon.request("POST", "/api", action.to_string().as_bytes()).await?;
    if status != 200 {
        bail!("{}", String::from_utf8_lossy(&body));
    }
    Ok(vec![json!({"type": "text", "text": "done"})])
}

async fn call(daemon: &Daemon, name: &str, args: &Value) -> Result<Vec<Value>> {
    let num = |key: &str| args[key].as_f64().with_context(|| format!("missing number '{key}'"));
    let text = |key: &str| args[key].as_str().with_context(|| format!("missing string '{key}'"));
    match name {
        "screenshot" => {
            let (status, jpeg) = daemon.request("GET", "/frame.jpg?settled=true", b"").await?;
            if status != 200 {
                frame_size(daemon).await?;
                bail!("no picture from the phone yet");
            }
            let (w, h) = frame_size(daemon).await?;
            Ok(vec![
                json!({
                    "type": "image",
                    "mimeType": "image/jpeg",
                    "data": base64::engine::general_purpose::STANDARD.encode(jpeg)
                }),
                json!({"type": "text", "text": format!("{w}x{h} pixels")}),
            ])
        }
        "tap" => {
            let (w, h) = frame_size(daemon).await?;
            act(daemon, json!({"t": "tap", "x": num("x")? / w, "y": num("y")? / h})).await
        }
        "swipe" => {
            let (w, h) = frame_size(daemon).await?;
            let action = json!({
                "t": "swipe",
                "x1": num("x1")? / w, "y1": num("y1")? / h,
                "x2": num("x2")? / w, "y2": num("y2")? / h,
                "ms": args["duration_ms"].as_u64()
            });
            act(daemon, action).await
        }
        "home" => act(daemon, json!({"t": "home"})).await,
        "app_switcher" => act(daemon, json!({"t": "apps"})).await,
        "list_elements" => {
            let elements = elements(daemon).await?;
            let lines: Vec<String> = elements
                .iter()
                .map(|e| match centre(e) {
                    Some((x, y)) => format!("({x}, {y})  {}", e["label"].as_str().unwrap_or_default()),
                    None => format!("(off screen)  {}", e["label"].as_str().unwrap_or_default()),
                })
                .collect();
            Ok(vec![json!({"type": "text", "text": lines.join("\n")})])
        }
        "tap_element" => {
            let wanted = text("label")?.to_lowercase();
            let elements = elements(daemon).await?;
            let label = |e: &Value| e["label"].as_str().unwrap_or_default().to_lowercase();
            let visible: Vec<&Value> = elements.iter().filter(|e| centre(e).is_some()).collect();
            let element = visible
                .iter()
                .find(|e| label(e).starts_with(&wanted))
                .or_else(|| visible.iter().find(|e| label(e).contains(&wanted)))
                .with_context(|| format!("nothing visible on screen is labelled '{wanted}'"))?;
            let (x, y) = centre(element).unwrap();
            let (w, h) = frame_size(daemon).await?;
            act(daemon, json!({"t": "tap", "x": x as f64 / w, "y": y as f64 / h})).await?;
            let tapped = format!("tapped '{}' at ({x}, {y})", element["label"].as_str().unwrap_or_default());
            Ok(vec![json!({"type": "text", "text": tapped})])
        }
        "list_apps" => {
            let apps: Vec<Value> = serde_json::from_slice(&post(daemon, json!({"t": "installed_apps"})).await?)?;
            let lines: Vec<String> = apps
                .iter()
                .map(|a| format!("{}  ({})", a["name"].as_str().unwrap_or_default(), a["bundle_id"].as_str().unwrap_or_default()))
                .collect();
            Ok(vec![json!({"type": "text", "text": lines.join("\n")})])
        }
        "launch_app" => {
            let opened: Value = serde_json::from_slice(&post(daemon, json!({"t": "launch", "app": text("app")?})).await?)?;
            let name = opened["name"].as_str().unwrap_or_default();
            Ok(vec![json!({"type": "text", "text": format!("opened {name}")})])
        }
        "start_recording" | "stop_recording" => {
            let reply: Value = serde_json::from_slice(
                &post(daemon, json!({"t": "record", "on": name == "start_recording"})).await?,
            )?;
            let path = reply["path"].as_str().unwrap_or_default();
            let text = if name == "start_recording" { format!("recording to {path}") } else { format!("saved {path}") };
            Ok(vec![json!({"type": "text", "text": text})])
        }
        "set_location" => {
            act(daemon, json!({"t": "location", "lat": num("latitude")?, "lon": num("longitude")?})).await
        }
        "clear_location" => act(daemon, json!({"t": "location_off"})).await,
        "app_performance" => {
            let sample: Value = serde_json::from_slice(&post(daemon, json!({"t": "perf"})).await?)?;
            let text = format!(
                "{}: CPU {:.0}%, memory {:.0} MB, {:.0} fps",
                sample["app"].as_str().unwrap_or_default(),
                sample["cpu"].as_f64().unwrap_or(0.0),
                sample["memory_mb"].as_f64().unwrap_or(0.0),
                sample["fps"].as_f64().unwrap_or(0.0),
            );
            Ok(vec![json!({"type": "text", "text": text})])
        }
        "back" | "spotlight" | "control_center" | "notifications" => {
            act(daemon, json!({"t": name})).await
        }
        "pinch" => {
            let (w, h) = frame_size(daemon).await?;
            let action = json!({"t": "pinch", "x": num("x")? / w, "y": num("y")? / h, "scale": num("scale")?});
            act(daemon, action).await
        }
        "press_button" => act(daemon, json!({"t": "button", "name": text("button")?})).await,
        "type_text" => act(daemon, json!({"t": "text", "text": text("text")?})).await,
        "press_key" => act(daemon, json!({"t": "key", "name": text("key")?})).await,
        _ => bail!("unknown tool '{name}'"),
    }
}

pub async fn run(usb: bool) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Notifications carry no id and get no reply.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let params = &message["params"];
        let result = match message["method"].as_str().unwrap_or_default() {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "ilinkos", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Controls the user's real iPhone over Wi-Fi. Take a screenshot \
                    before acting and after each action to confirm what happened."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let outcome = match daemon::ensure(usb).await {
                    Ok(daemon) => {
                        let name = params["name"].as_str().unwrap_or_default();
                        call(&daemon, name, &params["arguments"]).await
                    }
                    Err(e) => Err(e),
                };
                Ok(match outcome {
                    Ok(content) => json!({"content": content}),
                    Err(e) => json!({
                        "content": [{"type": "text", "text": format!("{e:#}")}],
                        "isError": true
                    }),
                })
            }
            other => Err(json!({"code": -32601, "message": format!("unknown method '{other}'")})),
        };
        let reply = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        stdout.write_all(format!("{reply}\n").as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}
