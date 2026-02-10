use std::sync::Arc;

use cucumber::gherkin::Step;
use cucumber::{given, then, when};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::world::{
    self, TestWorld, ADMIN_PASSWORD, ADMIN_USERNAME, BOT_PASSWORD, BOT_USERNAME,
    OBSERVER_PASSWORD, OBSERVER_USERNAME,
};

fn http_client() -> reqwest::Client {
    reqwest::Client::new()
}

// -- Given steps --

#[given("a running Matrix homeserver")]
async fn a_running_matrix_homeserver(world: &mut TestWorld) {
    if world.synapse_container.is_some() {
        return;
    }
    let (container, port) = world::start_synapse().await;
    world.synapse_container = Some(container);
    world.synapse_port = port;

    let http = http_client();

    // Register admin user
    world.admin_access_token = world::register_user_via_shared_secret(
        &http,
        world.synapse_port,
        "admin",
        "admin_password",
        true,
    )
    .await;

    // Register bot user
    world::register_user_via_shared_secret(
        &http,
        world.synapse_port,
        BOT_USERNAME,
        BOT_PASSWORD,
        false,
    )
    .await;

    // Register observer user
    world.observer_access_token = world::register_user_via_shared_secret(
        &http,
        world.synapse_port,
        OBSERVER_USERNAME,
        OBSERVER_PASSWORD,
        false,
    )
    .await;

    // Register issue admin user (for command tests)
    world.issue_admin_access_token = world::register_user_via_shared_secret(
        &http,
        world.synapse_port,
        ADMIN_USERNAME,
        ADMIN_PASSWORD,
        false,
    )
    .await;
}

#[given("a running PostgreSQL database")]
async fn a_running_postgres(world: &mut TestWorld) {
    if world.postgres_container.is_some() {
        return;
    }
    let (container, port) = world::start_postgres().await;
    world.postgres_container = Some(container);
    world.postgres_port = port;
}

#[given("the bot is started and connected to Matrix")]
async fn the_bot_is_started(world: &mut TestWorld) {
    if world.bot_handle.is_some() {
        return;
    }

    let synapse_port = world.synapse_port;
    let postgres_port = world.postgres_port;

    // Find a free port for the webhook server
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind ephemeral port");
    let webhook_port = listener.local_addr().unwrap().port();
    drop(listener);

    world.webhook_port = webhook_port;

    // Start wiremock server for Seerr API
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/api/v1/issue/\d+/comment"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0..)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"/api/v1/issue/\d+/resolved"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0..)
        .mount(&mock_server)
        .await;

    let seerr_api_url = mock_server.uri();
    world.seerr_mock = Some(Arc::new(mock_server));

    let homeserver_url = format!("http://localhost:{synapse_port}");
    let database_url = format!(
        "postgres://testuser:testpass@localhost:{postgres_port}/homelab_bot_test"
    );
    let listen_addr = format!("127.0.0.1:{webhook_port}");
    let admin_user_id = format!("@{ADMIN_USERNAME}:localhost");

    let handle = tokio::spawn(async move {
        let config = homelab_bot::config::Config {
            matrix_homeserver_url: homeserver_url,
            matrix_user_id: BOT_USERNAME.to_string(),
            matrix_password: BOT_PASSWORD.to_string(),
            matrix_room_alias: "#support_hoohoot:localhost".to_string(),
            database_url,
            webhook_listen_addr: listen_addr,
            seerr_api_url,
            seerr_api_key: "test-api-key".to_string(),
            matrix_admin_users: vec![admin_user_id],
        };

        let pool = sqlx::PgPool::connect(&config.database_url)
            .await
            .expect("Failed to connect to DB");
        homelab_bot::db::run_migrations(&pool)
            .await
            .expect("Failed to run migrations");

        let client = homelab_bot::matrix::create_and_login(
            &config.matrix_homeserver_url,
            &config.matrix_user_id,
            &config.matrix_password,
        )
        .await
        .expect("Failed to login bot");

        let (room, _room_id) = homelab_bot::matrix::join_room(&client, &config.matrix_room_alias)
            .await
            .expect("Failed to join room");

        let seerr_client =
            homelab_bot::seerr_client::SeerrClient::new(&config.seerr_api_url, &config.seerr_api_key);

        let admin_users: Vec<matrix_sdk::ruma::OwnedUserId> = config
            .matrix_admin_users
            .iter()
            .filter_map(|u| matrix_sdk::ruma::OwnedUserId::try_from(u.as_str()).ok())
            .collect();

        let cmd_ctx = std::sync::Arc::new(homelab_bot::commands::CommandContext {
            db: pool.clone(),
            seerr_client,
            admin_users,
        });

        client.add_event_handler_context(cmd_ctx);
        client.add_event_handler(homelab_bot::commands::on_room_message);

        let state = std::sync::Arc::new(homelab_bot::AppState { room, db: pool });

        let app = axum::Router::new()
            .route(
                "/webhook/seerr",
                axum::routing::post(homelab_bot::webhook::handle_seerr_webhook),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(&config.webhook_listen_addr)
            .await
            .expect("Failed to bind");

        let sync_client = client.clone();
        tokio::select! {
            result = axum::serve(listener, app) => {
                result.expect("Server error");
            }
            _ = sync_client.sync(matrix_sdk::config::SyncSettings::default()) => {}
        }
    });

    world.bot_handle = Some(handle);

    // Give the bot time to start
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
}

#[given(expr = "a room {string} exists")]
async fn a_room_exists(world: &mut TestWorld, room_alias: String) {
    let http = http_client();
    // Extract local part from alias (e.g., "#support_hoohoot:localhost" -> "support_hoohoot")
    let local_part = room_alias
        .trim_start_matches('#')
        .split(':')
        .next()
        .unwrap_or(&room_alias);

    let bot_user_id = format!("@{BOT_USERNAME}:localhost");
    let observer_user_id = format!("@{OBSERVER_USERNAME}:localhost");
    let issue_admin_user_id = format!("@{ADMIN_USERNAME}:localhost");

    world.room_id = world::create_room(
        &http,
        world.synapse_port,
        &world.admin_access_token,
        local_part,
        &[&bot_user_id, &observer_user_id, &issue_admin_user_id],
    )
    .await;

    // Bot and observer need to join the room
    // The bot will auto-join via the main logic, but for observer we join explicitly
    let _: serde_json::Value = http
        .post(format!(
            "http://localhost:{}/_matrix/client/v3/join/{}",
            world.synapse_port, world.room_id
        ))
        .bearer_auth(&world.observer_access_token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("Observer failed to join room")
        .json()
        .await
        .expect("Failed to parse join response");

    // Issue admin also joins
    let _: serde_json::Value = http
        .post(format!(
            "http://localhost:{}/_matrix/client/v3/join/{}",
            world.synapse_port, world.room_id
        ))
        .bearer_auth(&world.issue_admin_access_token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("Issue admin failed to join room")
        .json()
        .await
        .expect("Failed to parse join response");

    // Small delay to let the room sync
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}

// -- When steps --

#[given(regex = r#"^Seerr sends an "([^"]*)" webhook with:$"#)]
#[when(regex = r#"^Seerr sends an "([^"]*)" webhook with:$"#)]
async fn seerr_sends_webhook(world: &mut TestWorld, step: &Step, notification_type: String) {
    let data = world::table_to_map(step);
    let http = http_client();

    let payload = serde_json::json!({
        "notification_type": notification_type,
        "subject": data.get("subject").cloned().unwrap_or_default(),
        "message": data.get("message").cloned(),
        "image": data.get("image").cloned(),
        "issue_id": data.get("issue_id").cloned(),
        "reported_by": data.get("reported_by").cloned(),
        "comment": data.get("comment").cloned(),
        "commented_by": data.get("commented_by").cloned(),
    });

    let resp = http
        .post(format!(
            "http://127.0.0.1:{}/webhook/seerr",
            world.webhook_port
        ))
        .json(&payload)
        .send()
        .await
        .expect("Failed to send webhook");

    assert!(
        resp.status().is_success(),
        "Webhook returned error: {}",
        resp.status()
    );

    // Give Matrix time to process the message
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}

// -- Then steps --

#[given(regex = r#"^a message appears in "[^"]*" containing "([^"]*)"$"#)]
#[then(regex = r#"^a message appears in "[^"]*" containing "([^"]*)"$"#)]
async fn message_appears_containing(world: &mut TestWorld, expected_text: String) {
    let http = http_client();
    let messages = world::sync_and_find_messages(
        &http,
        world.synapse_port,
        &world.observer_access_token,
        &world.room_id,
    )
    .await;

    let found = messages.iter().find(|msg| {
        let body = msg["content"]["body"].as_str().unwrap_or("");
        let formatted = msg["content"]["formatted_body"].as_str().unwrap_or("");
        body.contains(&expected_text) || formatted.contains(&expected_text)
    });

    assert!(
        found.is_some(),
        "No message containing '{}' found in room. Messages: {:?}",
        expected_text,
        messages
            .iter()
            .map(|m| m["content"]["body"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
    );

    // Store the event ID of the found message as the root for thread assertions
    if let Some(msg) = found {
        if let Some(event_id) = msg["event_id"].as_str() {
            world.last_root_event_id = event_id.to_string();
        }
    }
}

#[then(regex = r#"^the message contains "([^"]*)"$"#)]
async fn the_message_contains(world: &mut TestWorld, expected_text: String) {
    let http = http_client();
    let messages = world::sync_and_find_messages(
        &http,
        world.synapse_port,
        &world.observer_access_token,
        &world.room_id,
    )
    .await;

    let found = messages.iter().any(|msg| {
        let event_id = msg["event_id"].as_str().unwrap_or("");
        if event_id != world.last_root_event_id {
            return false;
        }
        let body = msg["content"]["body"].as_str().unwrap_or("");
        let formatted = msg["content"]["formatted_body"].as_str().unwrap_or("");
        body.contains(&expected_text) || formatted.contains(&expected_text)
    });

    assert!(
        found,
        "Root message does not contain '{expected_text}'"
    );
}

#[given(regex = r#"^a threaded reply appears on the original message containing "([^"]*)"$"#)]
#[then(regex = r#"^a threaded reply appears on the original message containing "([^"]*)"$"#)]
async fn threaded_reply_appears(world: &mut TestWorld, expected_text: String) {
    let http = http_client();

    let thread_messages = world::get_relations(
        &http,
        world.synapse_port,
        &world.observer_access_token,
        &world.room_id,
        &world.last_root_event_id,
        "m.thread",
    )
    .await;

    let found = thread_messages.iter().find(|msg| {
        let body = msg["content"]["body"].as_str().unwrap_or("");
        let formatted = msg["content"]["formatted_body"].as_str().unwrap_or("");
        body.contains(&expected_text) || formatted.contains(&expected_text)
    });

    assert!(
        found.is_some(),
        "No threaded reply containing '{}' found. Thread messages: {:?}",
        expected_text,
        thread_messages
            .iter()
            .map(|m| m["content"]["body"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
    );

    if let Some(msg) = found {
        if let Some(event_id) = msg["event_id"].as_str() {
            world.last_thread_event_id = event_id.to_string();
        }
    }
}

#[then(regex = r#"^the threaded reply contains "([^"]*)"$"#)]
async fn threaded_reply_contains(world: &mut TestWorld, expected_text: String) {
    let http = http_client();

    let thread_messages = world::get_relations(
        &http,
        world.synapse_port,
        &world.observer_access_token,
        &world.room_id,
        &world.last_root_event_id,
        "m.thread",
    )
    .await;

    let found = thread_messages.iter().any(|msg| {
        let event_id = msg["event_id"].as_str().unwrap_or("");
        if event_id != world.last_thread_event_id {
            return false;
        }
        let body = msg["content"]["body"].as_str().unwrap_or("");
        let formatted = msg["content"]["formatted_body"].as_str().unwrap_or("");
        body.contains(&expected_text) || formatted.contains(&expected_text)
    });

    assert!(
        found,
        "Threaded reply does not contain '{expected_text}'"
    );
}

#[given(regex = r#"^the original message has a "([^"]*)" reaction$"#)]
#[then(regex = r#"^the original message has a "([^"]*)" reaction$"#)]
async fn has_reaction(world: &mut TestWorld, emoji: String) {
    let http = http_client();

    let reactions = world::get_relations(
        &http,
        world.synapse_port,
        &world.observer_access_token,
        &world.room_id,
        &world.last_root_event_id,
        "m.annotation",
    )
    .await;

    let found = reactions.iter().any(|r| {
        r["content"]["m.relates_to"]["key"].as_str() == Some(emoji.as_str())
    });

    assert!(
        found,
        "No '{emoji}' reaction found on the original message. Reactions: {reactions:?}"
    );
}

#[then(regex = r#"^the original message no longer has a "([^"]*)" reaction$"#)]
async fn no_longer_has_reaction(world: &mut TestWorld, emoji: String) {
    let http = http_client();

    let reactions = world::get_relations(
        &http,
        world.synapse_port,
        &world.observer_access_token,
        &world.room_id,
        &world.last_root_event_id,
        "m.annotation",
    )
    .await;

    let found = reactions.iter().any(|r| {
        r["content"]["m.relates_to"]["key"].as_str() == Some(emoji.as_str())
    });

    assert!(
        !found,
        "'{emoji}' reaction still exists on the original message"
    );
}

// -- Admin command steps --

#[when(regex = r#"^the admin sends '([^']*)' as a thread reply$"#)]
async fn admin_sends_thread_reply(world: &mut TestWorld, command: String) {
    let http = http_client();
    let root_event_id = &world.last_root_event_id;

    let body = serde_json::json!({
        "msgtype": "m.text",
        "body": command,
        "m.relates_to": {
            "rel_type": "m.thread",
            "event_id": root_event_id,
            "m.in_reply_to": {
                "event_id": root_event_id,
            },
            "is_falling_back": true,
        },
    });

    let resp: serde_json::Value = http
        .put(format!(
            "http://localhost:{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            world.synapse_port,
            world.room_id,
            format!("txn-admin-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()),
        ))
        .bearer_auth(&world.issue_admin_access_token)
        .json(&body)
        .send()
        .await
        .expect("Failed to send admin thread reply")
        .json()
        .await
        .expect("Failed to parse send response");

    assert!(
        resp["event_id"].as_str().is_some(),
        "Failed to send admin command: {resp:?}"
    );

    // Give the bot time to process the command via sync
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
}

#[then(regex = r#"^Seerr received a comment "([^"]*)" for issue (\d+)$"#)]
async fn seerr_received_comment(world: &mut TestWorld, comment: String, issue_id: u64) {
    let mock_server = world.seerr_mock.as_ref().expect("Wiremock not started");

    let received = mock_server.received_requests().await.expect("No requests recorded");
    let expected_path = format!("/api/v1/issue/{}/comment", issue_id);

    let found = received.iter().any(|req| {
        if req.url.path() != expected_path {
            return false;
        }
        if let Ok(body) = serde_json::from_slice::<serde_json::Value>(&req.body) {
            body["message"].as_str() == Some(comment.as_str())
        } else {
            false
        }
    });

    assert!(
        found,
        "Seerr did not receive comment '{comment}' for issue {issue_id}. Received requests: {:?}",
        received.iter().map(|r| format!("{} {}", r.method, r.url.path())).collect::<Vec<_>>()
    );
}

#[then(regex = r#"^Seerr received a resolve request for issue (\d+)$"#)]
async fn seerr_received_resolve(world: &mut TestWorld, issue_id: u64) {
    let mock_server = world.seerr_mock.as_ref().expect("Wiremock not started");

    let received = mock_server.received_requests().await.expect("No requests recorded");
    let expected_path = format!("/api/v1/issue/{}/resolved", issue_id);

    let found = received.iter().any(|req| req.url.path() == expected_path);

    assert!(
        found,
        "Seerr did not receive resolve request for issue {issue_id}. Received requests: {:?}",
        received.iter().map(|r| format!("{} {}", r.method, r.url.path())).collect::<Vec<_>>()
    );
}
