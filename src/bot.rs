use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use teloxide::{
    prelude::*,
    types::{KeyboardButton, KeyboardMarkup, Message, ReplyMarkup},
    RequestError,
};
use reqwest::Client;
use uuid::Uuid;

const SECTION_INTERN:    &str = "Intern/Junior Job";
const SECTION_MID:       &str = "Mid/Senior Job";
const SECTION_NEWS:      &str = "News";
const SECTION_ACTIVITY:  &str = "Activity";
const SECTION_TOKEN:     &str = "Token Analysis";
const CANCEL:            &str = "❌ Cancel";

const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const ALLOWED_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

#[derive(Clone)]
enum PendingAction {
    Content {
        category: String,
        token_symbol: Option<String>,
    },
    AwaitTokenSymbol,
}

type WaitingMap = Arc<Mutex<HashMap<ChatId, PendingAction>>>;

struct BotConfig {
    bot_api_secret: String,
    api_url: String,
    public_dir: PathBuf,
    allowed_users: Vec<i64>,
}

impl BotConfig {
    fn from_env() -> Self {
        let bot_api_secret = env::var("BOT_API_SECRET").expect("BOT_API_SECRET must be set");
        let api_url = env::var("API_URL").unwrap_or_else(|_| "http://localhost:3001/api/add-content".to_string());
        let public_dir = PathBuf::from(env::var("PUBLIC_DIR").unwrap_or_else(|_| "../frontend/public/images".to_string()));
        let allowed_users = env::var("ALLOWED_USERS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| s.trim().parse::<i64>().ok())
            .collect();

        Self { bot_api_secret, api_url, public_dir, allowed_users }
    }

    fn is_allowed(&self, chat_id: ChatId) -> bool {
        if self.allowed_users.is_empty() {
            return false;
        }
        self.allowed_users.contains(&chat_id.0)
    }
}

fn main_keyboard() -> ReplyMarkup {
    let keyboard = vec![
        vec![KeyboardButton::new(SECTION_INTERN), KeyboardButton::new(SECTION_MID)],
        vec![KeyboardButton::new(SECTION_NEWS), KeyboardButton::new(SECTION_ACTIVITY)],
        vec![KeyboardButton::new(SECTION_TOKEN)],
        vec![KeyboardButton::new(CANCEL)],
    ];
    ReplyMarkup::Keyboard(KeyboardMarkup::new(keyboard).resize_keyboard())
}

fn safe_extension(telegram_path: &str) -> &'static str {
    let ext = Path::new(telegram_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    ALLOWED_EXTENSIONS
        .iter()
        .find(|&&allowed| allowed == ext.as_str())
        .copied()
        .unwrap_or("jpg")
}

fn safe_save_path(public_dir: &Path, filename: &str) -> Option<PathBuf> {
    let p = Path::new(filename);
    if p.components().count() != 1 {
        return None;
    }
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !ALLOWED_EXTENSIONS.contains(&ext) {
        return None;
    }
    Some(public_dir.join(filename))
}

async fn download_and_save_photo(
    bot: &Bot,
    telegram_client: &Client,
    file_id: &str,
    public_dir: &Path,
) -> Result<String, String> {
    let file = bot.get_file(file_id).await
        .map_err(|e| format!("Telegram file error: {}", e))?;

    let ext = safe_extension(&file.path);
    let filename = format!("{}.{}", Uuid::new_v4(), ext);

    let save_path = safe_save_path(public_dir, &filename)
        .ok_or_else(|| "Invalid file path".to_string())?;

    if let Some(parent) = save_path.parent() {
        tokio::fs::create_dir_all(parent).await
            .map_err(|e| format!("Cannot create dir: {}", e))?;
    }

    let file_url = format!(
        "https://api.telegram.org/file/bot{}/{}",
        bot.token(),
        file.path
    );

    let response = telegram_client.get(&file_url).send().await
        .map_err(|e| format!("Network error: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("Telegram returned HTTP {}", response.status()));
    }

    let bytes = response.bytes().await
        .map_err(|e| format!("Download error: {}", e))?;

    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!("File too large ({} MB, max {} MB)", bytes.len() / 1024 / 1024, MAX_IMAGE_BYTES / 1024 / 1024));
    }

    tokio::fs::write(&save_path, &bytes).await
        .map_err(|e| format!("File save error: {}", e))?;

    Ok(format!("/images/{}", filename))
}

pub async fn start_bot() {
    let bot_token = env::var("BOT_TOKEN").expect("BOT_TOKEN must be set");
    let bot = Bot::new(bot_token);
    let config = Arc::new(BotConfig::from_env());

    let telegram_client = Arc::new(Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("Failed to build Telegram HTTP client"));

    let local_client = Arc::new(Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("Failed to build local HTTP client"));

    let waiting: WaitingMap = Arc::new(Mutex::new(HashMap::new()));

    let handler = {
        let config = config.clone();
        let telegram_client = telegram_client.clone();
        let local_client = local_client.clone();
        let waiting = waiting.clone();

        Update::filter_message()
            .branch(dptree::endpoint(
                move |msg: Message, bot: Bot| {
                    let config = config.clone();
                    let telegram_client = telegram_client.clone();
                    let local_client = local_client.clone();
                    let waiting = waiting.clone();

                    async move {
                        let chat_id = msg.chat.id;

                        if !config.is_allowed(chat_id) {
                            return Ok::<_, RequestError>(());
                        }

                        let text = msg.text().or(msg.caption()).unwrap_or("").to_string();

                        if text.len() > 65_536 {
                            bot.send_message(chat_id, "❌ Message too long (max 64KB)").await?;
                            return Ok::<_, RequestError>(());
                        }

                        if text == CANCEL {
                            waiting.lock().unwrap().remove(&chat_id);
                            bot.send_message(chat_id, "Action cancelled. Choose a section:")
                                .reply_markup(main_keyboard())
                                .await?;
                            return Ok::<_, RequestError>(());
                        }

                        let pending = {
                            let lock = waiting.lock().unwrap();
                            lock.get(&chat_id).cloned()
                        };

                        if let Some(pending) = pending {
                            match pending {
                                PendingAction::Content { category, token_symbol } => {
                                    if text.is_empty() && msg.photo().is_none() {
                                        bot.send_message(chat_id, "Please send text (or photo with caption)").await?;
                                        return Ok::<_, RequestError>(());
                                    }

                                    let mut image_url: Option<String> = None;
                                    if let Some(photo) = msg.photo().and_then(|p| p.last()) {
                                        match download_and_save_photo(&bot, &telegram_client, &photo.file.id, &config.public_dir).await {
                                            Ok(url) => {
                                                image_url = Some(url);
                                            }
                                            Err(e) => {
                                                bot.send_message(chat_id, format!("❌ {}", e)).await?;
                                                return Ok::<_, RequestError>(());
                                            }
                                        }
                                    }

                                    let endpoint = match category.as_str() {
                                        "intern" | "mid" => {
                                            config.api_url.replace("add-content", "add-job")
                                        }
                                        _ => config.api_url.clone(),
                                    };

                                    let payload = serde_json::json!({
                                        "text": text,
                                        "category": category,
                                        "token_symbol": token_symbol,
                                        "image_url": image_url.unwrap_or_default(),
                                    });

                                    match local_client
                                        .post(&endpoint)
                                        .header("x-bot-token", &config.bot_api_secret)
                                        .header("content-type", "application/json")
                                        .json(&payload)
                                        .send()
                                        .await
                                    {
                                        Ok(resp) if resp.status().is_success() => {
                                            bot.send_message(chat_id, "✅ Posted!").await?;
                                        }
                                        Ok(resp) => {
                                            let status = resp.status();
                                            let body = resp.text().await.unwrap_or_else(|_| "unknown".into());
                                            eprintln!("API error {}: {}", status, body);
                                            bot.send_message(chat_id, format!("❌ Server error (HTTP {})", status)).await?;
                                        }
                                        Err(e) => {
                                            eprintln!("Local API request failed: {}", e);
                                            bot.send_message(chat_id, "❌ Failed to reach API. Try again later.").await?;
                                        }
                                    }

                                    waiting.lock().unwrap().remove(&chat_id);
                                    bot.send_message(chat_id, "Choose next action:")
                                        .reply_markup(main_keyboard())
                                        .await?;
                                }

                                PendingAction::AwaitTokenSymbol => {
                                    if text.is_empty() {
                                        bot.send_message(chat_id, "Please type the token symbol (e.g. BTC):").await?;
                                    } else {
                                        let symbol = text.trim().to_uppercase();
                                        if symbol.len() > 20 || !symbol.chars().all(|c| c.is_alphanumeric()) {
                                            bot.send_message(chat_id, "❌ Invalid symbol. Use only letters and digits (e.g. BTC, ETH2):").await?;
                                            return Ok::<_, RequestError>(());
                                        }

                                        bot.send_message(chat_id, format!("Now send text + optional photo for ${}", symbol)).await?;

                                        waiting.lock().unwrap().insert(
                                            chat_id,
                                            PendingAction::Content {
                                                category: "token_analysis".into(),
                                                token_symbol: Some(symbol),
                                            },
                                        );
                                    }
                                }
                            }
                        } else {
                            let (prompt, category) = match text.as_str() {
                                SECTION_INTERN => ("Send text + optional photo for Intern/Junior job:", "intern"),
                                SECTION_MID => ("Send text + optional photo for Mid/Senior job:", "mid"),
                                SECTION_NEWS => ("Send text + optional photo for a news post:", "news"),
                                SECTION_ACTIVITY => ("Send text + optional photo for an activity:", "activity"),
                                SECTION_TOKEN => {
                                    bot.send_message(chat_id, "First, type the token symbol (e.g. BTC):").await?;
                                    waiting.lock().unwrap().insert(chat_id, PendingAction::AwaitTokenSymbol);
                                    return Ok::<_, RequestError>(());
                                }
                                _ => {
                                    bot.send_message(chat_id, "Choose a section:")
                                        .reply_markup(main_keyboard())
                                        .await?;
                                    return Ok::<_, RequestError>(());
                                }
                            };

                            bot.send_message(chat_id, prompt).await?;
                            waiting.lock().unwrap().insert(
                                chat_id,
                                PendingAction::Content {
                                    category: category.to_string(),
                                    token_symbol: None,
                                },
                            );
                        }

                        Ok::<_, RequestError>(())
                    }
                },
            ))
    };

    Dispatcher::builder(bot, handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
