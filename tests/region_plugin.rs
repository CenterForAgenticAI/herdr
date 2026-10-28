#![cfg(unix)]

//! End-to-end region lifecycle against a real server and the shell-script fixture plugin.

pub mod support;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid,
};

const PLUGIN_ID: &str = "example.region";

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/hreg-{}-{nanos}", std::process::id()))
}

struct SpawnedHerdr {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        support::stop_spawned_herdr(&mut *self.child);
        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            unregister_spawned_herdr_pid(Some(pid));
        }
    }
}

fn spawn_herdr(config_home: &Path, runtime_dir: &Path, socket_path: &Path) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(
        config_home.join("herdr/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    support::isolate_herdr_test_process(&mut cmd);
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", socket_path);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");
    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not appear at {}", path.display());
}

fn request(socket_path: &Path, method: &str, params: serde_json::Value) -> serde_json::Value {
    let mut stream = UnixStream::connect(socket_path).unwrap();
    let line = serde_json::json!({"id": "req", "method": method, "params": params});
    stream.write_all(line.to_string().as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while byte[0] != b'\n' {
        let n = stream.read(&mut byte).expect("response line");
        assert!(n > 0, "server closed the connection before replying");
        buf.push(byte[0]);
    }
    serde_json::from_slice(&buf).unwrap()
}

fn result(response: serde_json::Value) -> serde_json::Value {
    assert!(
        response.get("error").is_none(),
        "unexpected error: {response}"
    );
    response["result"].clone()
}

fn region_list(socket_path: &Path) -> Vec<serde_json::Value> {
    result(request(socket_path, "region.list", serde_json::json!({})))["regions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Pid the fixture script wrote to its pidfile, once it has started.
fn fixture_pid(pidfile: &Path) -> Option<u32> {
    fs::read_to_string(pidfile).ok()?.trim().parse().ok()
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    done()
}

/// Copies the fixture into a unique directory so concurrent runs never share a script path.
fn stage_fixture(base: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-region");
    let target = base.join("plugin-region");
    fs::create_dir_all(&target).unwrap();
    for name in ["herdr-plugin.toml", "region.sh"] {
        fs::copy(source.join(name), target.join(name)).unwrap();
    }
    target
}

#[test]
fn fixture_region_survives_tab_changes_and_closes_when_plugin_is_disabled() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let plugin_dir = stage_fixture(&base);
    let pidfile = base.join("region.pid");

    let server = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = result(request(
        &socket_path,
        "workspace.create",
        serde_json::json!({"cwd": base.display().to_string(), "focus": true}),
    ));
    let workspace_id = created["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let first_tab_id = created["workspace"]["active_tab_id"]
        .as_str()
        .unwrap()
        .to_string();

    result(request(
        &socket_path,
        "plugin.link",
        serde_json::json!({"path": plugin_dir.display().to_string(), "enabled": true}),
    ));

    let opened = result(request(
        &socket_path,
        "region.open",
        serde_json::json!({
            "plugin_id": PLUGIN_ID,
            "entrypoint": "board",
            "env": {"REGION_FIXTURE_PIDFILE": pidfile.display().to_string()},
        }),
    ));
    let region_id = opened["region"]["region_id"]
        .as_str()
        .expect("region id")
        .to_string();
    assert_eq!(opened["region"]["size"], 30);
    assert_eq!(opened["region"]["visible"], true);

    assert!(
        wait_until(Duration::from_secs(5), || fixture_pid(&pidfile).is_some()),
        "region process did not start"
    );
    let region_pid = fixture_pid(&pidfile).expect("region pid");
    assert!(process_alive(region_pid));

    // The region is not a pane: it never appears in pane.list.
    let panes = result(request(
        &socket_path,
        "pane.list",
        serde_json::json!({"workspace_id": workspace_id}),
    ));
    assert!(!panes.to_string().contains(&region_id), "{panes}");

    // A new tab and switching tabs leave the region and its process in place.
    let tab = result(request(
        &socket_path,
        "tab.create",
        serde_json::json!({"workspace_id": workspace_id, "focus": true}),
    ));
    assert_eq!(tab["type"], "tab_created");
    result(request(
        &socket_path,
        "tab.focus",
        serde_json::json!({"tab_id": first_tab_id}),
    ));
    let regions = region_list(&socket_path);
    assert_eq!(regions.len(), 1, "{regions:?}");
    assert_eq!(regions[0]["region_id"], region_id.as_str());
    assert!(
        process_alive(region_pid),
        "region process stopped on tab change"
    );

    // Disabling the plugin closes the region and stops its process.
    result(request(
        &socket_path,
        "plugin.disable",
        serde_json::json!({"plugin_id": PLUGIN_ID}),
    ));
    assert!(region_list(&socket_path).is_empty());
    assert!(
        wait_until(Duration::from_secs(5), || !process_alive(region_pid)),
        "region process {region_pid} survived plugin disable"
    );

    drop(server);
    cleanup_test_base(&base);
}
