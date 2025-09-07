use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use lazy_static::lazy_static;
use parking_lot::RwLock;
use rocket::fs::{FileServer, relative};
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};
use rocket::{Request};
use rocket::serde::{Deserialize, Serialize};
use rocket::form::{Form, FromForm};
use rocket::response::Redirect;
use rocket::http::CookieJar;
use rocket_dyn_templates::{Template, context};
use rocket::uri;
use serde_json::json;
use uuid::Uuid;
use ws::{listen, Handler, Sender, Message, Handshake, CloseCode};

// Data structures
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMessage {
    id: String,
    room_id: String,
    sender: String,
    content: String,
    timestamp: String,
    message_type: MessageType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum MessageType {
    UserMessage,
    SystemMessage,
    Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct User {
    id: String,
    nickname: String,
    room_id: String,
}

// Global state
struct ChatState {
    rooms: RwLock<HashMap<String, RoomState>>,
}

#[derive(Clone)]
struct RoomState {
    users: Arc<RwLock<HashMap<String, User>>>,
    messages: Arc<RwLock<Vec<ChatMessage>>>,
    connections: Arc<RwLock<Vec<Sender>>>,
}

impl RoomState {
    fn new() -> Self {
        RoomState {
            users: Arc::new(RwLock::new(HashMap::new())),
            messages: Arc::new(RwLock::new(Vec::new())),
            connections: Arc::new(RwLock::new(Vec::new())),
        }
    }

    fn broadcast(&self, msg: &str) {
        let connections = self.connections.read();
        for connection in connections.iter() {
            let _ = connection.send(msg);
        }
    }
}

impl ChatState {
    fn new() -> Self {
        ChatState {
            rooms: RwLock::new(HashMap::new()),
        }
    }

    fn get_or_create_room(&self, room_id: &str) -> RoomState {
        let mut rooms = self.rooms.write();
        if !rooms.contains_key(room_id) {
            rooms.insert(room_id.to_string(), RoomState::new());
        }
        rooms.get(room_id).unwrap().clone()
    }
}

lazy_static! {
    static ref CHAT_STATE: ChatState = ChatState::new();
}

// Form data
#[derive(FromForm)]
struct NicknameForm {
    nickname: String,
}

// Request guards
struct UserSession {
    user_id: String,
    nickname: String,
    room_id: String,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for UserSession {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let cookies = request.cookies();

        if let (Some(user_id), Some(nickname), Some(room_id)) = (
            cookies.get_private("user_id").map(|c| c.value().to_string()),
            cookies.get_private("nickname").map(|c| c.value().to_string()),
            cookies.get_private("room_id").map(|c| c.value().to_string()),
        ) {
            Outcome::Success(UserSession {
                user_id,
                nickname,
                room_id,
            })
        } else {
            Outcome::Forward(Status::SeeOther)
        }
    }
}

// Routes
#[rocket::get("/?<rid>")]
fn index(rid: Option<&str>, user_session: Option<UserSession>) -> Template {
    let room_id = rid.unwrap_or("lobby").to_string();

    match user_session {
        Some(session) if session.room_id == room_id => {
            Template::render("chat", context! {
                room_id: room_id.clone(),
                nickname: session.nickname,
                title: format!("Chat Room: {}", room_id),
                ws_path: format!("/{}", room_id),
            })
        },
        _ => {
            Template::render("login", context! {
                room_id: room_id.clone(),
                title: format!("Join Room: {}", room_id),
            })
        }
    }
}

#[rocket::post("/?<rid>", data = "<form>")]
fn login(rid: Option<&str>, form: Form<NicknameForm>, cookies: &CookieJar<'_>) -> Redirect {
    let room_id = rid.unwrap_or("lobby").to_string();
    let nickname = form.nickname.clone();

    // Check if the nickname is already taken in this room
    let room_state = CHAT_STATE.get_or_create_room(&room_id);
    let users = room_state.users.read();

    if users.values().any(|user| user.nickname == nickname && user.room_id == room_id) {
        // Nickname is taken, redirect back to log in
        return Redirect::to(uri!(index(Some(&room_id))));
    }

    // Set cookies
    let user_id = Uuid::new_v4().to_string();
    cookies.add_private(rocket::http::Cookie::new("user_id", user_id.clone()));
    cookies.add_private(rocket::http::Cookie::new("nickname", nickname.clone()));
    cookies.add_private(rocket::http::Cookie::new("room_id", room_id.clone()));

    // Add user to room
    drop(users); // Release the read lock before acquiring write lock
    let mut users = room_state.users.write();
    users.insert(user_id.clone(), User {
        id: user_id,
        nickname: nickname.clone(),
        room_id: room_id.clone(),
    });

    // Add a system message
    let mut messages = room_state.messages.write();
    messages.push(ChatMessage {
        id: Uuid::new_v4().to_string(),
        room_id: room_id.clone(),
        sender: "System".to_string(),
        content: format!("{} has joined the room", nickname),
        timestamp: DateTime::<Utc>::from(SystemTime::now()).to_rfc3339(),
        message_type: MessageType::SystemMessage,
    });

    // Broadcast the join message
    room_state.broadcast(&json!({
        "type": "system",
        "content": format!("{} has joined the room", nickname)
    }).to_string());

    Redirect::to(uri!(index(Some(&room_id))))
}

#[rocket::get("/logout")]
fn logout(user_session: Option<UserSession>, cookies: &CookieJar<'_>) -> Redirect {
    if let Some(session) = user_session {
        // Remove user from room
        let room_state = CHAT_STATE.get_or_create_room(&session.room_id);
        let mut users = room_state.users.write();
        users.remove(&session.user_id);

        // Add a system message
        let mut messages = room_state.messages.write();
        messages.push(ChatMessage {
            id: Uuid::new_v4().to_string(),
            room_id: session.room_id.clone(),
            sender: "System".to_string(),
            content: format!("{} has left the room", session.nickname),
            timestamp: DateTime::<Utc>::from(SystemTime::now()).to_rfc3339(),
            message_type: MessageType::SystemMessage,
        });

        // Broadcast the leave message
        room_state.broadcast(&json!({
            "type": "system",
            "content": format!("{} has left the room", session.nickname)
        }).to_string());

        // Clear cookies
        cookies.remove_private("user_id");
        cookies.remove_private("nickname");
        cookies.remove_private("room_id");
    }

    Redirect::to(uri!(index(None::<&str>)))
}

// WebSocket handler
struct ChatSocketHandler {
    sender: Sender,
    room_id: String,
    user_id: String,
    nickname: String,
}

impl ChatSocketHandler {
    fn new(sender: Sender, handshake: &Handshake) -> Self {
        // Extract room_id from URL path
        let path = handshake.request.resource();
        let room_id = if path.starts_with('/') && path.len() > 1 {
            path[1..].to_string() // Remove leading '/'
        } else {
            "lobby".to_string()
        };

        // Parse cookies to get user info
        let mut user_id = Uuid::new_v4().to_string();
        let mut nickname = format!("User-{}", sender.connection_id());

        // Try to extract user info from cookies
        if let Some(cookie_header) = handshake.request.header("Cookie") {
            if let Ok(cookie_str) = std::str::from_utf8(cookie_header) {
                for cookie in cookie_str.split(';') {
                    let parts: Vec<&str> = cookie.trim().split('=').collect();
                    if parts.len() == 2 {
                        match parts[0] {
                            "user_id" => user_id = parts[1].to_string(),
                            "nickname" => nickname = parts[1].to_string(),
                            _ => {}
                        }
                    }
                }
            }
        }

        ChatSocketHandler {
            sender,
            room_id,
            user_id,
            nickname,
        }
    }
}

impl Handler for ChatSocketHandler {
    fn on_open(&mut self, handshake: Handshake) -> ws::Result<()> {
        // Update handler with handshake info if needed
        *self = ChatSocketHandler::new(self.sender.clone(), &handshake);
        let room_state = CHAT_STATE.get_or_create_room(&self.room_id);

        // Add connection to the room
        {
            let mut connections = room_state.connections.write();
            connections.push(self.sender.clone());
        }

        // Send message history to a new user
        {
            let messages = room_state.messages.read();
            for msg in messages.iter() {
                let _ = self.sender.send(json!({
                    "type": match msg.message_type {
                        MessageType::UserMessage => "message",
                        MessageType::SystemMessage => "system",
                        MessageType::Command => "command",
                    },
                    "id": msg.id,
                    "sender": msg.sender,
                    "content": msg.content,
                    "timestamp": msg.timestamp,
                }).to_string());
            }
        }

        // Add user to room if not already there
        {
            let mut users = room_state.users.write();
            if !users.contains_key(&self.user_id) {
                users.insert(self.user_id.clone(), User {
                    id: self.user_id.clone(),
                    nickname: self.nickname.clone(),
                    room_id: self.room_id.clone(),
                });

                // Add a system message
                let mut messages = room_state.messages.write();
                messages.push(ChatMessage {
                    id: Uuid::new_v4().to_string(),
                    room_id: self.room_id.clone(),
                    sender: "System".to_string(),
                    content: format!("{} has joined the room", self.nickname),
                    timestamp: DateTime::<Utc>::from(SystemTime::now()).to_rfc3339(),
                    message_type: MessageType::SystemMessage,
                });

                // Broadcast the join message
                room_state.broadcast(&json!({
                    "type": "system",
                    "content": format!("{} has joined the room", self.nickname)
                }).to_string());
            }
        }

        Ok(())
    }

    fn on_message(&mut self, msg: Message) -> ws::Result<()> {
        if let Ok(text) = msg.into_text() {
            // Parse the message
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                    let room_state = CHAT_STATE.get_or_create_room(&self.room_id);

                    // Check if it's a command
                    if content.starts_with('/') {
                        self.handle_command(content);
                    } else {
                        // Regular message
                        let msg = ChatMessage {
                            id: Uuid::new_v4().to_string(),
                            room_id: self.room_id.clone(),
                            sender: self.nickname.clone(),
                            content: content.to_string(),
                            timestamp: DateTime::<Utc>::from(SystemTime::now()).to_rfc3339(),
                            message_type: MessageType::UserMessage,
                        };

                        // Add to history
                        {
                            let mut messages = room_state.messages.write();
                            messages.push(msg.clone());
                        }

                        // Broadcast to all users in the room
                        room_state.broadcast(&json!({
                            "type": "message",
                            "id": msg.id,
                            "sender": msg.sender,
                            "content": msg.content,
                            "timestamp": msg.timestamp,
                        }).to_string());
                    }
                }
            }
        }

        Ok(())
    }

    fn on_close(&mut self, _: CloseCode, _: &str) {
        let room_state = CHAT_STATE.get_or_create_room(&self.room_id);

        // Remove connection from the room
        {
            let mut connections = room_state.connections.write();
            connections.retain(|conn| conn.connection_id() != self.sender.connection_id());
        }

        // Check if this was the last connection for this user
        let is_last_connection = {
            let connections = room_state.connections.read();
            connections.iter().filter(|conn| {
                // This is a simplification - in a real app, you'd need to track which connection belongs to which user
                conn.connection_id() == self.sender.connection_id()
            }).count() == 0
        };

        if is_last_connection {
            // Remove user from room
            {
                let mut users = room_state.users.write();
                users.remove(&self.user_id);
            }

            // Add a system message
            {
                let mut messages = room_state.messages.write();
                messages.push(ChatMessage {
                    id: Uuid::new_v4().to_string(),
                    room_id: self.room_id.clone(),
                    sender: "System".to_string(),
                    content: format!("{} has left the room", self.nickname),
                    timestamp: DateTime::<Utc>::from(SystemTime::now()).to_rfc3339(),
                    message_type: MessageType::SystemMessage,
                });
            }

            // Broadcast the leave message
            room_state.broadcast(&json!({
                "type": "system",
                "content": format!("{} has left the room", self.nickname)
            }).to_string());
        }
    }
}

impl ChatSocketHandler {
    fn handle_command(&self, command: &str) {
        match command {
            "/clear" => {
                // Clear messages for this user only
                let _ = self.sender.send(json!({
                    "type": "command",
                    "command": "clear"
                }).to_string());
            },
            "/logout" => {
                // Tell the client to redirect to log out
                let _ = self.sender.send(json!({
                    "type": "command",
                    "command": "logout"
                }).to_string());
            },
            _ => {
                // Unknown command
                let _ = self.sender.send(json!({
                    "type": "system",
                    "content": format!("Unknown command: {}", command)
                }).to_string());
            }
        }
    }
}

// Start a WebSocket server in a separate thread
fn start_websocket_server() {
    thread::spawn(|| {
        listen("0.0.0.0:8082", |out| {
            ChatSocketHandler {
                sender: out,
                room_id: String::new(), // Will be set in on_open
                user_id: String::new(), // Will be set in on_open
                nickname: String::new(), // Will be set in on_open
            }
        }).unwrap();
    });
}

#[rocket::launch]
fn rocket() -> _ {
    // Start WebSocket server
    start_websocket_server();

    // Create a templates directory if it doesn't exist
    std::fs::create_dir_all("templates").ok();

    // Create a static directory if it doesn't exist
    std::fs::create_dir_all("static").ok();

    // Create login template
    let login_template = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>{{ title }}</title>
    <style>
        body {
            font-family: Arial, sans-serif;
            margin: 0;
            padding: 0;
            display: flex;
            justify-content: center;
            align-items: center;
            height: 100vh;
            background-color: #f5f5f5;
        }
        .login-container {
            background-color: white;
            padding: 2rem;
            border-radius: 8px;
            box-shadow: 0 2px 10px rgba(0, 0, 0, 0.1);
            width: 100%;
            max-width: 400px;
        }
        h1 {
            margin-top: 0;
            color: #333;
        }
        form {
            display: flex;
            flex-direction: column;
        }
        input {
            padding: 0.8rem;
            margin-bottom: 1rem;
            border: 1px solid #ddd;
            border-radius: 4px;
            font-size: 1rem;
        }
        button {
            padding: 0.8rem;
            background-color: #4CAF50;
            color: white;
            border: none;
            border-radius: 4px;
            font-size: 1rem;
            cursor: pointer;
        }
        button:hover {
            background-color: #45a049;
        }
        @media (max-width: 480px) {
            .login-container {
                width: 90%;
                padding: 1.5rem;
            }
        }
    </style>
</head>
<body>
    <div class="login-container">
        <h1>{{ title }}</h1>
        <form method="post">
            <input type="text" name="nickname" placeholder="Enter your nickname" required autofocus>
            <button type="submit">Join Chat</button>
        </form>
    </div>
</body>
</html>"#;

    // Create a chat template
    let chat_template = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>{{ title }}</title>
    <style>
        body {
            font-family: Arial, sans-serif;
            margin: 0;
            padding: 0;
            display: flex;
            flex-direction: column;
            height: 100vh;
            background-color: #f5f5f5;
        }
        .chat-container {
            display: flex;
            flex-direction: column;
            height: 100%;
            max-width: 1200px;
            margin: 0 auto;
            width: 100%;
            background-color: white;
            box-shadow: 0 2px 10px rgba(0, 0, 0, 0.1);
        }
        .chat-header {
            padding: 1rem;
            background-color: #4CAF50;
            color: white;
            display: flex;
            justify-content: space-between;
            align-items: center;
        }
        .chat-header h1 {
            margin: 0;
            font-size: 1.5rem;
        }
        .chat-header a {
            color: white;
            text-decoration: none;
        }
        .chat-messages {
            flex: 1;
            overflow-y: auto;
            padding: 1rem;
        }
        .message {
            margin-bottom: 1rem;
            padding: 0.8rem;
            border-radius: 4px;
            max-width: 80%;
        }
        .message.user {
            background-color: #e6f7ff;
            align-self: flex-end;
            margin-left: auto;
        }
        .message.system {
            background-color: #f0f0f0;
            color: #666;
            font-style: italic;
            text-align: center;
            max-width: 100%;
        }
        .message .sender {
            font-weight: bold;
            margin-bottom: 0.3rem;
        }
        .message .time {
            font-size: 0.8rem;
            color: #999;
            margin-top: 0.3rem;
        }
        .chat-input {
            display: flex;
            padding: 1rem;
            border-top: 1px solid #eee;
        }
        .chat-input input {
            flex: 1;
            padding: 0.8rem;
            border: 1px solid #ddd;
            border-radius: 4px;
            font-size: 1rem;
            margin-right: 0.5rem;
        }
        .chat-input button {
            padding: 0.8rem 1.5rem;
            background-color: #4CAF50;
            color: white;
            border: none;
            border-radius: 4px;
            font-size: 1rem;
            cursor: pointer;
        }
        .chat-input button:hover {
            background-color: #45a049;
        }
        @media (max-width: 768px) {
            .chat-header h1 {
                font-size: 1.2rem;
            }
            .message {
                max-width: 90%;
            }
        }
    </style>
</head>
<body>
    <div class="chat-container">
        <div class="chat-header">
            <h1>{{ title }}</h1>
            <a href="/logout">Logout</a>
        </div>
        <div class="chat-messages" id="messages"></div>
        <div class="chat-input">
            <input type="text" id="message-input" placeholder="Type a message..." autocomplete="off">
            <button id="send-button">Send</button>
        </div>
    </div>

    <script>
        const nickname = "{{ nickname }}";
        const roomId = "{{ room_id }}";
        const wsPath = "{{ ws_path }}";
        // Use port 8082 for WebSocket connections
        const wsUrl = "ws://" + window.location.hostname + ":8082" + wsPath;

        let ws;

        function connect() {
            ws = new WebSocket(wsUrl);

            ws.onopen = function() {
                console.log("Connected to WebSocket");
            };

            ws.onmessage = function(event) {
                const data = JSON.parse(event.data);

                if (data.type === "command") {
                    handleCommand(data);
                } else {
                    addMessage(data);
                }
            };

            ws.onclose = function() {
                console.log("Disconnected from WebSocket");
                // Try to reconnect after a delay
                setTimeout(connect, 3000);
            };

            ws.onerror = function(error) {
                console.error("WebSocket error:", error);
            };
        }

        function handleCommand(data) {
            switch (data.command) {
                case "clear":
                    document.getElementById("messages").innerHTML = "";
                    break;
                case "logout":
                    window.location.href = "/logout";
                    break;
            }
        }

        function addMessage(data) {
            const messagesDiv = document.getElementById("messages");
            const messageDiv = document.createElement("div");

            messageDiv.className = `message ${data.type}`;

            if (data.type === "message") {
                const senderDiv = document.createElement("div");
                senderDiv.className = "sender";
                senderDiv.textContent = data.sender;
                messageDiv.appendChild(senderDiv);

                const contentDiv = document.createElement("div");
                contentDiv.className = "content";
                contentDiv.textContent = data.content;
                messageDiv.appendChild(contentDiv);

                const timeDiv = document.createElement("div");
                timeDiv.className = "time";
                timeDiv.textContent = new Date(data.timestamp).toLocaleTimeString();
                messageDiv.appendChild(timeDiv);
            } else if (data.type === "system") {
                messageDiv.textContent = data.content;
            }

            messagesDiv.appendChild(messageDiv);
            messagesDiv.scrollTop = messagesDiv.scrollHeight;
        }

        document.getElementById("send-button").addEventListener("click", sendMessage);
        document.getElementById("message-input").addEventListener("keypress", function(e) {
            if (e.key === "Enter") {
                sendMessage();
            }
        });

        function sendMessage() {
            const input = document.getElementById("message-input");
            const message = input.value.trim();

            if (message && ws && ws.readyState === WebSocket.OPEN) {
                ws.send(JSON.stringify({
                    content: message
                }));

                input.value = "";
            } else if (message && (!ws || ws.readyState !== WebSocket.OPEN)) {
                console.log("WebSocket not connected, attempting to reconnect...");
                connect();
            }
        }

        // Connect to WebSocket when page loads
        connect();
    </script>
</body>
</html>"#;

    // Write templates to files
    std::fs::write("templates/login.html.hbs", login_template).ok();
    std::fs::write("templates/chat.html.hbs", chat_template).ok();

    rocket::build()
        .mount("/", rocket::routes![index, login, logout])
        .mount("/static", FileServer::from(relative!("static")))
        .attach(Template::fairing())
}
