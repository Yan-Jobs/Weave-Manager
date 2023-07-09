#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::ffi::OsStr;
use std::sync::{Mutex, Arc};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, Child};
use std::{fs, io};
use std::env;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::fs::File;
use std::collections::HashMap;
use serde::{Serialize, Deserialize};
use serde_json;

use sysinfo::{Pid, PidExt, ProcessExt, ProcessRefreshKind, System, SystemExt};
use tauri::{Manager, State, AppHandle, SystemTrayEvent};
use tauri::{SystemTray, SystemTrayMenu, CustomMenuItem, SystemTrayMenuItem};
use zip::result::ZipError;
use zip::ZipArchive;
use chrono::prelude::Local;
use tauri::api::path::home_dir;

#[derive(Serialize)]
enum ClientType {
    LunarClient,
    Forge,
    Vanilla
}

#[derive(Serialize)]
struct MinecraftInstance {
    pid: u32,
    cmd: Vec<String>,
    cwd: String,
    version: String,
    start_time: u64,
    client_type: ClientType,
    weave_attached: bool
}

#[derive(Serialize, Deserialize)]
struct ModConfig {
    name: Option<String>,
    author: Option<String>,
    version: Option<String>,
    link: Option<String>,
}
#[derive(Debug, Deserialize, Serialize)]
struct Analytics {
    launch_times: [u32; 10],
    time_played: u64,
    average_launch_time: f32,
}

#[derive(Clone, Serialize)]
struct ConsolePayload {
    line: String,
    pid: u32
}

#[tauri::command]
fn get_weave_directory() -> PathBuf {
    let mut home = home_dir().unwrap();
    home.push(".weave");
    return home;
}

fn get_weave_logs_path() -> PathBuf {
    let mut weave_dir = get_weave_directory();
    weave_dir.push("logs");
    return weave_dir;
}

fn get_weave_loader_path() -> Option<PathBuf> {
    match home_dir() {
        Some(path) => {
            let weave_dir = path.join(".weave");

            if weave_dir.is_dir() {
                for entry in fs::read_dir(weave_dir).ok()? {
                    if let Ok(entry) = entry {
                        if entry.file_name().to_string_lossy().starts_with("Weave-Loader") {
                            return Some(entry.path())
                        }
                    }
                }
            }
        },
        None => eprintln!("Impossible to get your home dir"),
    }

    None
}

#[tauri::command]
fn read_mod_config(path: String) -> Option<ModConfig> {
    let f = File::open(&path).unwrap();
    let mut archive = ZipArchive::new(f).unwrap();
    let conf = match archive.by_name("weave.mod.json") {
        Ok(conf) => conf,
        Err(ZipError::FileNotFound) => return None,
        Err(e) => panic!("{:?}", e)
    };
    Some(serde_json::from_reader(conf).unwrap())
}

#[tauri::command]
fn fetch_minecraft_instances(app_state: State<AppState>) -> Vec<MinecraftInstance> {
    let mut system = app_state.system.lock().unwrap();
    system.refresh_processes_specifics(ProcessRefreshKind::new());

    system.processes().values()
        .filter_map(|proc| {
            if !matches!(proc.exe().file_name().and_then(OsStr::to_str), Some("javaw.exe" | "java")) {
                return None
            }

            if !proc.cmd().iter().any(|arg| arg.contains(".minecraft")) {
                return None
            }

            let client_type = if proc.cmd().iter().any(|arg| arg.contains("lunar")) {
                ClientType::LunarClient
            } else if proc.cmd().iter().any(|arg| arg.contains("minecraftforge")) {
                ClientType::Forge
            } else {
                ClientType::Vanilla
            };

            let weave_attached = if proc.cmd().iter().any(|arg| arg.contains("Weave-Loader")) {
                true
            } else {
                false
            };

            Some(MinecraftInstance {
                pid: proc.pid().as_u32(),
                cmd: proc.cmd().to_owned(),
                cwd: proc.cwd().to_string_lossy().to_string(),
                version: proc.cmd().iter().skip_while(|&arg| arg != "--version").nth(1)?.clone(),
                start_time: proc.start_time(),
                client_type,
                weave_attached
            })
        })
        .collect()
}

#[tauri::command]
fn relaunch_with_weave(cwd: String, cmd_line: Vec<String>, app_state: State<AppState>, app: tauri::AppHandle) {
    let weave_loader_path = get_weave_loader_path();

    let mut updated_cmd = cmd_line;

    if !weave_loader_path.is_none() {
        let java_agent = String::from("-javaagent:") + &weave_loader_path.unwrap().as_path().to_str().unwrap();
        updated_cmd.insert(1, java_agent);

        let mut _child = Command::new(&updated_cmd[0])
            .current_dir(Path::new(&cwd))
            .args(&updated_cmd[1..])
            .stdout(Stdio::piped())
            .spawn()
            .expect("Failed to relaunch with Weave");

        let stdout = _child.stdout.take().expect("Failed to capture stdout from child");
        let selected_arc = Arc::clone(&app_state.selected_process);

        let stdout_thread = std::thread::spawn(move || {
            let timestamp = Local::now().format("%Y-%m-%d-%H%M%S").to_string();

            let log_path = get_weave_logs_path().join(format!("{}.log", timestamp));
            let log_file = File::create(&log_path).expect("Failed to create log file");

            let reader = BufReader::new(stdout);
            let mut writer = BufWriter::with_capacity(1000, log_file);

            for line in reader.lines() {
                if let Ok(line) = line {
                    writer.write_all(format!("{}\n", line).as_bytes()).expect("Unable to write minecraft output to log file");

                    if _child.id() == *selected_arc.lock().unwrap() {
                        app.emit_all("console_output", ConsolePayload {
                            line,
                            pid: _child.id()
                        }).expect("Failed to emit console log to renderer");
                    }
                }
            }

            writer.flush().expect("Failed to flush BufWriter for log file");
        });
    }
}

#[tauri::command]
fn switch_console_output(pid: u32, app_state: State<AppState>) {
    *app_state.selected_process.lock().unwrap() = pid.into();
}

#[tauri::command]
fn kill_pid(pid: u32, app_state: State<AppState>) -> bool {
    app_state.system.lock().unwrap().process(Pid::from_u32(pid)).is_some_and(|p| p.kill())
}

#[tauri::command]
fn get_memory_usage(app_state: State<AppState>) -> (u64, u64) {
    let mut sys = app_state.system.lock().unwrap();
    sys.refresh_processes_specifics(ProcessRefreshKind::new());

    let total = sys.total_memory();
    let process = sys.process(sysinfo::get_current_pid().unwrap()).unwrap();
    let used = process.memory();

    (used, total)
}

#[tauri::command]
fn get_analytics() -> Analytics {
    let analytics_file = get_weave_directory().join("analytics.json");

    if let Ok(file_content) = fs::read_to_string(analytics_file) {
        if let Ok(analytics) = serde_json::from_str::<Analytics>(&file_content) {
            return Analytics {
                launch_times: analytics.launch_times,
                time_played: analytics.time_played,
                average_launch_time: analytics.average_launch_time
            }
        }
    }

    Analytics {
        launch_times: [0; 10],
        time_played: 0,
        average_launch_time: 0.0
    }
}

struct AppState {
    system: Mutex<System>,
    selected_process: Arc<Mutex<u32>>
}

fn main() {
    let app_state = AppState {
        system: Mutex::new(System::new_all()),
        selected_process: Arc::new(Mutex::new(0))
    };

    let tray_menu = SystemTrayMenu::new()
        .add_item(CustomMenuItem::new("show", "Show"))
        .add_native_item(SystemTrayMenuItem::Separator)
        .add_item(CustomMenuItem::new("quit", "Quit"));

    let tray = SystemTray::new().with_menu(tray_menu);

    tauri::Builder::default()
        .plugin(tauri_plugin_fs_watch::init())
        .system_tray(tray)
        .on_system_tray_event(|app, event| match event {
            SystemTrayEvent::LeftClick { .. } => {
                app.get_window("main").unwrap().show().unwrap()
            }
            SystemTrayEvent::MenuItemClick { id, .. } => {
                match id.as_str() {
                    "show" => app.get_window("main").unwrap().show().unwrap(),
                    "quit" => std::process::exit(0),
                    _ => {}
                }
            }
            _ => {}
        })
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            fetch_minecraft_instances,
            kill_pid,
            get_memory_usage,
            get_analytics,
            relaunch_with_weave,
            read_mod_config,
            switch_console_output
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}