use std::collections::HashMap;
use std::sync::Arc;

use cucumber::World;
use testcontainers::core::{ContainerAsync, ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::GenericImage;
use testcontainers::ImageExt;
use testcontainers_modules::postgres::Postgres;
use wiremock::MockServer;

pub const SYNAPSE_PORT: u16 = 8008;
pub const SHARED_SECRET: &str = "test-secret-key";
pub const BOT_USERNAME: &str = "bot";
pub const BOT_PASSWORD: &str = "bot_password";
pub const OBSERVER_USERNAME: &str = "observer";
pub const OBSERVER_PASSWORD: &str = "observer_password";
pub const ADMIN_USERNAME: &str = "issueadmin";
pub const ADMIN_PASSWORD: &str = "issueadmin_password";

#[derive(Debug, World)]
#[world(init = Self::new)]
pub struct TestWorld {
    pub synapse_container: Option<ContainerAsync<GenericImage>>,
    pub postgres_container: Option<ContainerAsync<Postgres>>,
    pub synapse_port: u16,
    pub postgres_port: u16,
    pub bot_handle: Option<tokio::task::JoinHandle<()>>,
    pub webhook_port: u16,
    pub observer_access_token: String,
    pub admin_access_token: String,
    pub room_id: String,
    pub last_root_event_id: String,
    pub last_thread_event_id: String,
    pub seerr_mock: Option<Arc<MockServer>>,
    pub issue_admin_access_token: String,
}

impl TestWorld {
    async fn new() -> Result<Self, anyhow::Error> {
        Ok(Self {
            synapse_container: None,
            postgres_container: None,
            synapse_port: 0,
            postgres_port: 0,
            bot_handle: None,
            webhook_port: 0,
            observer_access_token: String::new(),
            admin_access_token: String::new(),
            room_id: String::new(),
            last_root_event_id: String::new(),
            last_thread_event_id: String::new(),
            seerr_mock: None,
            issue_admin_access_token: String::new(),
        })
    }
}

pub async fn start_synapse() -> (ContainerAsync<GenericImage>, u16) {
    let homeserver_yaml = format!(
        r#"server_name: "localhost"
pid_file: /data/homeserver.pid
listeners:
  - port: {SYNAPSE_PORT}
    tls: false
    type: http
    bind_addresses: ['::']
    x_forwarded: false
    resources:
      - names: [client, federation]
        compress: false
database:
  name: sqlite3
  args:
    database: "/data/homeserver.db"
log_config: "/data/localhost.log.config"
media_store_path: "/data/media_store"
registration_shared_secret: "{SHARED_SECRET}"
enable_registration: true
enable_registration_without_verification: true
report_stats: false
macaroon_secret_key: "test-macaroon-secret-key"
form_secret: "test-form-secret"
signing_key_path: "/data/localhost.signing.key"
suppress_key_server_warning: true
"#
    );

    let log_config = r#"version: 1
formatters:
  precise:
    format: '%(asctime)s - %(name)s - %(lineno)d - %(levelname)s - %(request)s - %(message)s'
handlers:
  console:
    class: logging.StreamHandler
    formatter: precise
    stream: ext://sys.stderr
loggers:
  synapse.storage.SQL:
    level: WARN
root:
  level: INFO
  handlers: [console]
"#;

    let container = GenericImage::new("matrixdotorg/synapse", "latest")
        .with_exposed_port(ContainerPort::Tcp(SYNAPSE_PORT))
        .with_wait_for(WaitFor::message_on_stderr("SynapseSite starting on"))
        .with_copy_to(
            "/data/homeserver.yaml",
            homeserver_yaml.into_bytes(),
        )
        .with_copy_to(
            "/data/localhost.log.config",
            log_config.as_bytes().to_vec(),
        )
        .with_env_var("SYNAPSE_CONFIG_PATH", "/data/homeserver.yaml")
        .with_env_var("UID", "0")
        .with_env_var("GID", "0")
        .start()
        .await
        .expect("Failed to start Synapse container");

    let port = container
        .get_host_port_ipv4(SYNAPSE_PORT)
        .await
        .expect("Failed to get Synapse port");

    (container, port)
}

pub async fn start_postgres() -> (ContainerAsync<Postgres>, u16) {
    let container = Postgres::default()
        .with_db_name("homelab_bot_test")
        .with_user("testuser")
        .with_password("testpass")
        .start()
        .await
        .expect("Failed to start Postgres container");

    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("Failed to get Postgres port");

    (container, port)
}

pub async fn register_user_via_shared_secret(
    http: &reqwest::Client,
    synapse_port: u16,
    username: &str,
    password: &str,
    admin: bool,
) -> String {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;

    type HmacSha1 = Hmac<Sha1>;

    // Step 1: Get nonce
    let nonce_resp: serde_json::Value = http
        .get(format!(
            "http://localhost:{synapse_port}/_synapse/admin/v1/register"
        ))
        .send()
        .await
        .expect("Failed to get nonce")
        .json()
        .await
        .expect("Failed to parse nonce response");

    let nonce = nonce_resp["nonce"].as_str().expect("Missing nonce");

    // Step 2: Compute HMAC
    let admin_str = if admin { "admin" } else { "notadmin" };
    let mut mac =
        HmacSha1::new_from_slice(SHARED_SECRET.as_bytes()).expect("HMAC can take any key size");
    mac.update(nonce.as_bytes());
    mac.update(b"\x00");
    mac.update(username.as_bytes());
    mac.update(b"\x00");
    mac.update(password.as_bytes());
    mac.update(b"\x00");
    mac.update(admin_str.as_bytes());
    let mac_hex = hex::encode(mac.finalize().into_bytes());

    // Step 3: Register
    let register_resp: serde_json::Value = http
        .post(format!(
            "http://localhost:{synapse_port}/_synapse/admin/v1/register"
        ))
        .json(&serde_json::json!({
            "nonce": nonce,
            "username": username,
            "password": password,
            "admin": admin,
            "mac": mac_hex,
        }))
        .send()
        .await
        .expect("Failed to register user")
        .json()
        .await
        .expect("Failed to parse register response");

    register_resp["access_token"]
        .as_str()
        .expect("Missing access_token")
        .to_string()
}

pub async fn create_room(
    http: &reqwest::Client,
    synapse_port: u16,
    access_token: &str,
    alias_local: &str,
    invitees: &[&str],
) -> String {
    let resp: serde_json::Value = http
        .post(format!(
            "http://localhost:{synapse_port}/_matrix/client/v3/createRoom"
        ))
        .bearer_auth(access_token)
        .json(&serde_json::json!({
            "preset": "public_chat",
            "room_alias_name": alias_local,
            "name": alias_local,
            "invite": invitees,
        }))
        .send()
        .await
        .expect("Failed to create room")
        .json()
        .await
        .expect("Failed to parse createRoom response");

    resp["room_id"]
        .as_str()
        .expect("Missing room_id")
        .to_string()
}

pub fn table_to_map(step: &cucumber::gherkin::Step) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(table) = step.table.as_ref() {
        for row in &table.rows {
            if row.len() >= 2 {
                map.insert(row[0].trim().to_string(), row[1].trim().to_string());
            }
        }
    }
    map
}

pub async fn sync_and_find_messages(
    http: &reqwest::Client,
    synapse_port: u16,
    access_token: &str,
    room_id: &str,
) -> Vec<serde_json::Value> {
    let resp: serde_json::Value = http
        .get(format!(
            "http://localhost:{synapse_port}/_matrix/client/v3/rooms/{room_id}/messages"
        ))
        .bearer_auth(access_token)
        .query(&[("dir", "b"), ("limit", "50")])
        .send()
        .await
        .expect("Failed to get messages")
        .json()
        .await
        .expect("Failed to parse messages");

    resp["chunk"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

pub async fn get_relations(
    http: &reqwest::Client,
    synapse_port: u16,
    access_token: &str,
    room_id: &str,
    event_id: &str,
    rel_type: &str,
) -> Vec<serde_json::Value> {
    let resp: serde_json::Value = http
        .get(format!(
            "http://localhost:{synapse_port}/_matrix/client/v1/rooms/{room_id}/relations/{event_id}/{rel_type}"
        ))
        .bearer_auth(access_token)
        .send()
        .await
        .expect("Failed to get relations")
        .json()
        .await
        .expect("Failed to parse relations response");

    resp["chunk"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}
