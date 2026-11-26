//! An integration whose service takes one client at a time, as Intiface
//! Central does: changing its answers while it runs must still work.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use api::FieldValue;
use daemon::{ConfigService, CredentialStore, IntegrationService, PresencePause};

const ID: &str = "plugin:buttplug";
const SERVER_INFO: &str =
    r#"[{"ServerInfo":{"Id":1,"MaxPingTime":0,"MessageVersion":3,"ServerName":"one"}}]"#;

/// An unmasked text frame, as a server sends it.
fn text_frame(text: &str) -> Vec<u8> {
    let mut frame = vec![0x81, text.len() as u8];
    frame.extend_from_slice(text.as_bytes());
    frame
}

/// Answers the upgrade and the first message, then holds the line until the
/// client goes.
fn serve(mut stream: TcpStream, busy: Arc<AtomicBool>) {
    let mut buf = [0u8; 4096];
    let _ = stream.read(&mut buf);
    let _ = stream.write_all(
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
    );
    let _ = stream.read(&mut buf);
    let _ = stream.write_all(&text_frame(SERVER_INFO));
    while matches!(stream.read(&mut buf), Ok(read) if read > 0) {}
    busy.store(false, Ordering::SeqCst);
}

/// A server that turns a second client away while the first is connected.
fn one_client_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let busy = Arc::new(AtomicBool::new(false));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if busy.swap(true, Ordering::SeqCst) {
                drop(stream);
                continue;
            }
            let busy = Arc::clone(&busy);
            std::thread::spawn(move || serve(stream, busy));
        }
    });
    port
}

fn install_buttplug(dir: &std::path::Path) {
    let plugin = bundled_plugins::all()
        .iter()
        .find(|plugin| plugin.manifest.contains("id = \"buttplug\""))
        .expect("the buttplug plugin is bundled");
    let at = dir.join("buttplug");
    std::fs::create_dir_all(&at).unwrap();
    std::fs::write(at.join(plugin_host::MANIFEST_FILE), plugin.manifest).unwrap();
    std::fs::write(
        at.join(plugin_host::COMPONENT_FILE),
        plugin.wasm.expect("a component"),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn new_answers_are_checked_after_the_running_one_lets_go() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plugins = dir.path().join("plugins");
    install_buttplug(&plugins);
    server::plugins::init(Some(plugins), Vec::new()).await;

    let database = db::init(&dir.path().join("kopuz.db")).await.expect("db");
    let config = Arc::new(ConfigService::new(
        database.clone(),
        dir.path().join("settings.toml"),
        config::Settings::default(),
    ));
    let credentials = CredentialStore::load(&database).await.expect("credentials");
    let pause = PresencePause::load(&database).await.expect("pause");
    let service = IntegrationService::new(config, credentials, pause);

    let port = one_client_server();
    service
        .set_settings(
            ID,
            vec![
                FieldValue::new("$enabled", "true"),
                FieldValue::new("url", format!("ws://127.0.0.1:{port}")),
            ],
        )
        .await
        .expect("the first answers are checked while nothing holds the server");

    // Looking at the panel makes the running instance connect and hold it.
    let panels = service.panels().await;
    assert_eq!(panels.len(), 1);

    service
        .set_settings(
            ID,
            vec![FieldValue::new("url", format!("ws://127.0.0.1:{port}/"))],
        )
        .await
        .expect("new answers are checked once the running instance let go");
}
