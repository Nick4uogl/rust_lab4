use actix::prelude::*;
use actix_cors::Cors;
use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer};
use actix_web_actors::ws;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Serialize, Deserialize, Clone)]
struct ChatRoom {
    id: Uuid,
    name: String,
    created_by: String,
    participants: HashSet<String>,
    message_log: Vec<ChatMessage>,
}

#[derive(Default)]
struct SharedState {
    user_accounts: Mutex<HashMap<String, String>>, // username -> password
    chat_rooms: Mutex<HashMap<Uuid, ChatRoom>>,    // room_id -> ChatRoom
    active_sessions: Mutex<HashMap<Uuid, Vec<Addr<ClientSession>>>>, // room_id -> WebSocket connections
}

#[derive(Debug, Deserialize)]
struct UserRegistration {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct UserLogin {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct RoomCreation {
    name: String,
    creator: String,
}

#[derive(Deserialize)]
struct AddParticipant {
    room_id: Uuid,
    username: String,
}

#[derive(Deserialize, Message, Clone, Serialize)]
#[rtype(result = "()")]
struct ChatMessage {
    room_id: Uuid,
    sender: String,
    content: String,
}

// WebSocket Client Session
struct ClientSession {
    room_id: Uuid,
    username: String,
    state: Arc<SharedState>,
}

impl Actor for ClientSession {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        let mut sessions = self.state.active_sessions.lock().unwrap();
        sessions
            .entry(self.room_id)
            .or_default()
            .push(ctx.address());
    }

    fn stopped(&mut self, ctx: &mut Self::Context) {
        let mut sessions = self.state.active_sessions.lock().unwrap();
        if let Some(user_list) = sessions.get_mut(&self.room_id) {
            user_list.retain(|addr| addr != &ctx.address());
        }
    }
}

impl Handler<ChatMessage> for ClientSession {
    type Result = ();

    fn handle(&mut self, msg: ChatMessage, ctx: &mut Self::Context) {
        if msg.room_id == self.room_id {
            ctx.text(msg.content);
        }
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for ClientSession {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        if let Ok(ws::Message::Text(text)) = msg {
            let content = String::from_utf8_lossy(text.as_bytes()).to_string();

            let sessions = self.state.active_sessions.lock().unwrap();
            if let Some(users) = sessions.get(&self.room_id) {
                let new_message = ChatMessage {
                    room_id: self.room_id,
                    sender: self.username.clone(),
                    content: content.clone(),
                };

                // Broadcast to all users in the room
                for user in users {
                    user.do_send(new_message.clone());
                }

                // Save to room history
                let mut rooms = self.state.chat_rooms.lock().unwrap();
                if let Some(room) = rooms.get_mut(&self.room_id) {
                    room.message_log.push(new_message);
                }
            }
        } else {
            ctx.text("Received non-text message.");
        }
    }
}

// WebSocket entry point
async fn ws_handler(
    req: HttpRequest,
    stream: web::Payload,
    state: web::Data<Arc<SharedState>>,
) -> Result<HttpResponse, actix_web::Error> {
    let query: HashMap<String, String> = serde_urlencoded::from_str(req.query_string())
        .map_err(|_| actix_web::error::ErrorBadRequest("Invalid query"))?;

    let room_id = query
        .get("roomId")
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| actix_web::error::ErrorBadRequest("Invalid roomId"))?;

    let username = query
        .get("username")
        .cloned()
        .unwrap_or_else(|| "guest".to_string());

    ws::start(
        ClientSession {
            room_id,
            username,
            state: state.get_ref().clone(),
        },
        &req,
        stream,
    )
}

// REST Handlers
async fn register_user(
    state: web::Data<Arc<SharedState>>,
    form: web::Json<UserRegistration>,
) -> HttpResponse {
    let mut accounts = state.user_accounts.lock().unwrap();
    if accounts.contains_key(&form.username) {
        return HttpResponse::Conflict().body("User already exists");
    }
    accounts.insert(form.username.clone(), form.password.clone());
    HttpResponse::Ok().body("User registered successfully")
}

async fn login_user(
    state: web::Data<Arc<SharedState>>,
    form: web::Json<UserLogin>,
) -> HttpResponse {
    let accounts = state.user_accounts.lock().unwrap();
    if let Some(stored_pass) = accounts.get(&form.username) {
        if stored_pass == &form.password {
            return HttpResponse::Ok().body("Login successful");
        }
    }
    HttpResponse::Unauthorized().body("Invalid credentials")
}

async fn create_chat_room(
    state: web::Data<Arc<SharedState>>,
    form: web::Json<RoomCreation>,
) -> HttpResponse {
    let mut rooms = state.chat_rooms.lock().unwrap();
    let room = ChatRoom {
        id: Uuid::new_v4(),
        name: form.name.clone(),
        created_by: form.creator.clone(),
        participants: HashSet::new(),
        message_log: Vec::new(),
    };
    rooms.insert(room.id, room.clone());
    HttpResponse::Ok().json(room)
}

async fn add_participant(
    state: web::Data<Arc<SharedState>>,
    form: web::Json<AddParticipant>,
) -> HttpResponse {
    let mut rooms = state.chat_rooms.lock().unwrap();
    if let Some(room) = rooms.get_mut(&form.room_id) {
        room.participants.insert(form.username.clone());
        return HttpResponse::Ok().json(room.clone());
    }
    HttpResponse::NotFound().body("Room not found")
}

async fn list_chat_rooms(state: web::Data<Arc<SharedState>>) -> HttpResponse {
    let rooms = state.chat_rooms.lock().unwrap();
    let room_list: Vec<_> = rooms.values().cloned().collect();
    HttpResponse::Ok().json(room_list)
}

// Main function
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    std::env::set_var("RUST_LOG", "info");
    env_logger::init();

    let state = Arc::new(SharedState::default());

    HttpServer::new(move || {
        App::new()
            .wrap(
                Cors::default()
                    .allow_any_origin()
                    .allow_any_header()
                    .allow_any_method(),
            )
            .app_data(web::Data::new(state.clone()))
            .route("/register", web::post().to(register_user))
            .route("/login", web::post().to(login_user))
            .route("/create_room", web::post().to(create_chat_room))
            .route("/add_user", web::post().to(add_participant))
            .route("/list_rooms", web::get().to(list_chat_rooms))
            .route("/ws/", web::get().to(ws_handler))
    })
    .bind("127.0.0.1:8080")?
    .run()
    .await
}
