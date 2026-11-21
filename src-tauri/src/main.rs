#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{Manager, State};

type Shared = Arc<Mutex<Session>>;
type Result<T> = std::result::Result<T, String>;
#[derive(Deserialize, Serialize)]
struct Graph {
    id: String,
    title: String,
    series: Vec<String>,
}
#[derive(Deserialize)]
struct Field {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    unit: String,
}
#[derive(Deserialize)]
struct Control {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    min: Option<f64>,
    max: Option<f64>,
}
#[derive(Deserialize, Serialize)]
struct Camera {
    kind: String,
    preview_port: u16,
    record_port: u16,
}
#[derive(Deserialize)]
struct Descriptor {
    protocol_version: u32,
    max_rate_hz: u32,
    fields: Vec<Field>,
    graphs: Vec<Graph>,
    controls: Vec<Control>,
    cameras: Vec<Camera>,
}
#[derive(Default, Serialize)]
struct Frame {
    status: String,
    fields: Vec<Vec<String>>,
    controls: Vec<Vec<String>>,
    graphs: Option<Vec<Graph>>,
    camera: Option<Camera>,
    rows: VecDeque<(u64, Vec<String>)>,
    log: String,
    recording: String,
}
#[derive(Default)]
struct Session {
    generation: u64,
    address: Option<SocketAddr>,
    socket: Option<TcpStream>,
    frame: Frame,
    recorder: Option<(Child, PathBuf)>,
    record_enabled: bool,
}
impl Session {
    fn stop_video(&mut self) {
        if let Some((mut child, _)) = self.recorder.take() {
            // Matroska can be finalized cleanly by sending FFmpeg its quit command.
            if let Some(mut input) = child.stdin.take() {
                let _ = input.write_all(b"q\n");
            }
            thread::spawn(move || {
                for _ in 0..50 {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
            });
        }
        self.frame.recording.clear();
    }
    fn close(&mut self) {
        self.generation += 1;
        if let Some(sock) = self.socket.take() {
            let _ = sock.shutdown(Shutdown::Both);
        }
        self.stop_video();
        self.record_enabled = false;
        self.address = None;
    }
    fn start_video(&mut self) -> Result<()> {
        self.stop_video();
        let address = self.address.ok_or("Connect to a robot first")?;
        let port = self
            .frame
            .camera
            .as_ref()
            .ok_or("No recording endpoint advertised")?
            .record_port;
        if self.frame.log.is_empty() {
            self.frame.recording = "Waiting for telemetry".into();
            return Ok(());
        }
        let path = Path::new(&self.frame.log).with_extension(format!("{}.mkv", millis()));
        let child = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-rw_timeout",
                "5000000",
                "-i",
                &format!("http://{}:{port}", address.ip()),
                "-an",
                "-c:v",
                "copy",
                "-n",
            ])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                format!("Could not start FFmpeg. Install FFmpeg for raw video recording. ({e})")
            })?;
        self.recorder = Some((child, path));
        self.frame.recording = "Waiting for raw video".into();
        Ok(())
    }
}
fn millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn ioerr(e: std::io::Error) -> String {
    e.to_string()
}
fn socket(addr: SocketAddr) -> Result<TcpStream> {
    let sock = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(ioerr)?;
    sock.set_read_timeout(Some(Duration::from_secs(4)))
        .map_err(ioerr)?;
    sock.set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(ioerr)?;
    Ok(sock)
}
fn line(reader: &mut BufReader<TcpStream>) -> Result<String> {
    let mut s = String::new();
    if reader.read_line(&mut s).map_err(ioerr)? == 0 {
        return Err("Robot closed the connection".into());
    }
    Ok(s.trim_end_matches(['\r', '\n']).to_string())
}
fn catalog(s: &str, prefix: &str, width: usize) -> Result<Vec<Vec<String>>> {
    let payload = s.strip_prefix(prefix).ok_or("Unexpected robot response")?;
    if payload.is_empty() {
        return Ok(vec![]);
    }
    payload
        .split(';')
        .map(|entry| {
            let v: Vec<String> = entry.split(',').map(str::to_owned).collect();
            if v.len() != width || v[0].is_empty() {
                Err("Invalid field catalog".into())
            } else {
                Ok(v)
            }
        })
        .collect()
}
fn csv_line(file: &mut File, cells: impl IntoIterator<Item = String>) -> Result<()> {
    let row: Vec<_> = cells
        .into_iter()
        .map(|s| format!("\"{}\"", s.replace('"', "\"\"")))
        .collect();
    writeln!(file, "{}", row.join(",")).map_err(ioerr)
}
fn stream(shared: &Shared, id: u64, addr: SocketAddr, dir: PathBuf) -> Result<()> {
    let mut sock = socket(addr)?;
    {
        let mut s = shared.lock().unwrap();
        if s.generation != id {
            return Ok(());
        }
        s.socket = Some(sock.try_clone().map_err(ioerr)?);
    }
    let mut reader = BufReader::new(sock.try_clone().map_err(ioerr)?);
    sock.write_all(b"HOWDY\n").map_err(ioerr)?;
    let reply = line(&mut reader)?;
    let (fields, controls, graphs, camera, rate) =
        if let Some(json) = reply.strip_prefix("DESCRIBE ") {
            let d: Descriptor =
                serde_json::from_str(json).map_err(|e| format!("Invalid HOWDY descriptor: {e}"))?;
            if d.protocol_version != 2 || d.max_rate_hz == 0 {
                return Err("Unsupported HOWDY descriptor".into());
            }
            (
                d.fields
                    .into_iter()
                    .map(|f| vec![f.name, f.kind, f.unit])
                    .collect::<Vec<_>>(),
                d.controls
                    .into_iter()
                    .map(|c| {
                        vec![
                            c.name,
                            c.kind,
                            c.min.map(|v| v.to_string()).unwrap_or_default(),
                            c.max.map(|v| v.to_string()).unwrap_or_default(),
                        ]
                    })
                    .collect(),
                Some(d.graphs),
                d.cameras.into_iter().find(|c| c.kind == "limelight"),
                d.max_rate_hz.min(20),
            )
        } else if reply == "ERR unknown" {
            sock.write_all(b"FIELDS\nLISTCFG\n").map_err(ioerr)?;
            (
                catalog(&line(&mut reader)?, "FIELDS ", 3)?,
                catalog(&line(&mut reader)?, "CFG ", 4)?,
                None,
                Some(Camera {
                    kind: "limelight".into(),
                    preview_port: 5800,
                    record_port: 5802,
                }),
                20,
            )
        } else {
            return Err(format!("Unexpected HOWDY response: {reply}"));
        };
    if fields.is_empty() {
        return Err("Robot advertised no telemetry fields".into());
    }
    let run_index = fields.iter().position(|f| f[0] == "run_id");
    {
        let mut s = shared.lock().unwrap();
        if s.generation != id {
            return Ok(());
        }
        s.frame.fields = fields.clone();
        s.frame.controls = controls;
        s.frame.graphs = graphs;
        s.frame.camera = camera;
        s.frame.status = "Waiting for telemetry".into();
    }
    writeln!(sock, "SUB ALL rate={rate}").map_err(ioerr)?;
    sock.set_read_timeout(None).map_err(ioerr)?;
    let mut log: Option<(String, File)> = None;
    loop {
        let msg = match line(&mut reader) {
            Ok(m) => m,
            Err(e) => return Err(e),
        };
        let Some(data) = msg.strip_prefix("DATA ") else {
            continue;
        };
        let values: Vec<String> = data.split(',').map(str::to_owned).collect();
        if values.len() != fields.len() {
            return Err("Telemetry width differs from robot field catalog".into());
        }
        let now = millis();
        let run = run_index
            .and_then(|i| values.get(i))
            .map(String::as_str)
            .unwrap_or("");
        let mut s = shared.lock().unwrap();
        if s.generation != id {
            return Ok(());
        }
        if log.as_ref().map(|(r, _)| r.as_str()) != Some(run) {
            let label = run
                .parse::<i64>()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "session".into());
            let path = dir.join(format!("diag_{label}_{now}_{id}.csv"));
            let mut f = File::create_new(&path).map_err(ioerr)?;
            csv_line(
                &mut f,
                std::iter::once("host_ts_ms".into()).chain(fields.iter().map(|f| f[0].clone())),
            )?;
            s.frame.log = path.to_string_lossy().into();
            if s.record_enabled {
                if let Err(e) = s.start_video() {
                    s.frame.recording = e;
                    s.record_enabled = false;
                }
            }
            log = Some((run.to_owned(), f));
        }
        csv_line(
            &mut log.as_mut().unwrap().1,
            std::iter::once(now.to_string()).chain(values.iter().cloned()),
        )?;
        s.frame.status = "Live".into();
        if s.frame.rows.len() == 240 {
            s.frame.rows.pop_front();
        }
        s.frame.rows.push_back((now, values));
    }
}
fn logs(app: &tauri::AppHandle) -> Result<PathBuf> {
    let dir = app
        .path()
        .document_dir()
        .map_err(|e| e.to_string())?
        .join("EclipseDashboard")
        .join("Runs");
    fs::create_dir_all(&dir).map_err(ioerr)?;
    Ok(dir)
}
#[tauri::command]
fn connect(host: String, port: u16, state: State<Shared>, app: tauri::AppHandle) -> Result<()> {
    let ip: Ipv4Addr = host
        .trim()
        .parse()
        .map_err(|_| "Enter a valid IPv4 address")?;
    if port == 0 {
        return Err("Port must be between 1 and 65535".into());
    }
    let dir = logs(&app)?;
    let addr = SocketAddr::from((ip, port));
    let shared = state.inner().clone();
    let mut s = shared.lock().unwrap();
    s.close();
    let id = s.generation;
    s.address = Some(addr);
    s.frame = Frame {
        status: "Connecting".into(),
        ..Frame::default()
    };
    drop(s);
    thread::spawn(move || {
        if let Err(e) = stream(&shared, id, addr, dir) {
            let mut s = shared.lock().unwrap();
            if s.generation == id {
                s.close();
                s.frame.status = format!("Connection ended · {e}");
            }
        }
    });
    Ok(())
}
#[tauri::command]
fn disconnect(state: State<Shared>) {
    let mut s = state.lock().unwrap();
    s.close();
    s.frame = Frame::default();
}
#[tauri::command]
fn poll(state: State<Shared>) -> serde_json::Value {
    let mut s = state.lock().unwrap();
    if let Some((child, path)) = &mut s.recorder {
        if matches!(child.try_wait(), Ok(Some(_))) {
            s.recorder = None;
            s.record_enabled = false;
            s.frame.recording = "Raw stream ended · check camera and retry".into();
        } else if path.metadata().is_ok_and(|m| m.len() > 0) {
            s.frame.recording = "Recording".into();
        }
    }
    let result = serde_json::to_value(&s.frame).unwrap();
    s.frame.rows.clear();
    result
}
#[tauri::command]
fn record(enabled: bool, state: State<Shared>) -> Result<()> {
    let mut s = state.lock().unwrap();
    if enabled {
        s.start_video()?;
    } else {
        s.stop_video();
    }
    s.record_enabled = enabled;
    Ok(())
}
#[tauri::command]
async fn apply_setting(name: String, value: f64, state: State<'_, Shared>) -> Result<()> {
    let addr = {
        let s = state.lock().unwrap();
        let field = s
            .frame
            .controls
            .iter()
            .find(|v| v[0] == name)
            .ok_or("Unknown setting")?;
        if name.contains(['\r', '\n', '='])
            || !value.is_finite()
            || value < field[2].parse().unwrap_or(f64::NEG_INFINITY)
            || value > field[3].parse().unwrap_or(f64::INFINITY)
        {
            return Err("Value outside allowed range".into());
        }
        s.address.ok_or("Robot disconnected")?
    };
    tauri::async_runtime::spawn_blocking(move || {
        let mut sock = socket(addr)?;
        writeln!(sock, "SET {name}={value}").map_err(ioerr)?;
        if line(&mut BufReader::new(sock))? == "OK" {
            Ok(())
        } else {
            Err("Robot rejected value".into())
        }
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
fn open_runs(app: tauri::AppHandle) -> Result<()> {
    let dir = logs(&app)?;
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(program).arg(dir).spawn().map_err(ioerr)?;
    Ok(())
}
fn main() {
    tauri::Builder::default()
        .manage(Shared::default())
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::CloseRequested { .. }) {
                window.state::<Shared>().lock().unwrap().close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            connect,
            disconnect,
            poll,
            apply_setting,
            record,
            open_runs
        ])
        .run(tauri::generate_context!())
        .expect("desktop runtime");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    #[test]
    fn discovers_legacy_catalog_and_logs_separate_runs() {
        check_session(false);
    }

    #[test]
    fn reads_howdy_layout_and_limits_subscription_rate() {
        check_session(true);
    }

    fn check_session(howdy: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut client, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(client.try_clone().unwrap());
            assert_eq!(line(&mut reader).unwrap(), "HOWDY");
            if howdy {
                let descriptor = serde_json::json!({
                    "protocol_version":2, "max_rate_hz":10,
                    "fields":[{"name":"run_id","type":"long","unit":"s"},{"name":"servo_position","type":"double","unit":""}],
                    "graphs":[{"id":"servo","title":"Servo command","series":["servo_position"]}],
                    "controls":[{"name":"servo","type":"double","min":0,"max":1}],
                    "cameras":[{"kind":"limelight","preview_port":6000,"record_port":6002}]
                });
                writeln!(client, "DESCRIBE {descriptor}").unwrap();
            } else {
                client.write_all(b"ERR unknown\n").unwrap();
                assert_eq!(line(&mut reader).unwrap(), "FIELDS");
                assert_eq!(line(&mut reader).unwrap(), "LISTCFG");
                client
                    .write_all(
                        b"FIELDS run_id,long,s;servo_position,double,\nCFG servo,double,0,1\n",
                    )
                    .unwrap();
            }
            assert_eq!(
                line(&mut reader).unwrap(),
                if howdy {
                    "SUB ALL rate=10"
                } else {
                    "SUB ALL rate=20"
                }
            );
            client.write_all(b"OK\nDATA 1,0.25\nDATA 2,0.75\n").unwrap();
        });
        let dir = std::env::temp_dir().join(format!("eclipse-dashboard-test-{howdy}-{}", millis()));
        fs::create_dir(&dir).unwrap();
        let shared = Shared::default();
        assert!(stream(&shared, 0, addr, dir.clone())
            .unwrap_err()
            .contains("closed"));
        server.join().unwrap();
        let s = shared.lock().unwrap();
        if howdy {
            assert_eq!(s.frame.graphs.as_ref().unwrap()[0].title, "Servo command");
            assert_eq!(s.frame.camera.as_ref().unwrap().record_port, 6002);
        }
        assert_eq!(s.frame.rows.len(), 2);
        assert_eq!(s.frame.controls[0][0], "servo");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        assert!(fs::read_to_string(&s.frame.log)
            .unwrap()
            .contains("\"2\",\"0.75\""));
        fs::remove_dir_all(dir).unwrap();
    }
}
