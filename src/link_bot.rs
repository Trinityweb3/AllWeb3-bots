use std::env;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::InputFile;
use sqlx::PgPool;
use regex::Regex;
use chrono::{DateTime, Utc};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

pub async fn start_link_bot(pool: PgPool) {
    let bot_token = env::var("LINK_BOT_TOKEN").expect("LINK_BOT_TOKEN must be set");
    let bot = Bot::new(bot_token);

    let allowed_users: Vec<i64> = env::var("LINK_BOT_ALLOWED_USERS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();

    let url_regex = Regex::new(r"https?://[^\s]+").unwrap();
    let pool = Arc::new(pool);

    let pinger_pool = pool.clone();
    let pinger_bot = bot.clone();
    let pinger_users = allowed_users.clone();

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(86400)).await;
            if let Err(e) = check_404_links(&pinger_bot, &pinger_pool, &pinger_users).await {
                eprintln!("Link 404 pinger error: {}", e);
            }
        }
    });

    let handler = Update::filter_message().branch(dptree::endpoint(
        move |msg: Message, bot: Bot| {
            let pool = pool.clone();
            let allowed_users = allowed_users.clone();
            let url_regex = url_regex.clone();

            async move {
                let chat_id = msg.chat.id;

                if !allowed_users.contains(&chat_id.0) {
                    return Ok::<_, teloxide::RequestError>(());
                }

                let text = msg.text().unwrap_or("").trim().to_string();

                if text == "/start" {
                    bot.send_message(chat_id, "Дорогой Вазима, команды: /export, /check <ссылка>. Можно просто скидывать ссылки, я сохраню их и проверю на дубли.").await?;
                    return Ok::<_, teloxide::RequestError>(());
                }

                if text == "/export" {
                    export_links_to_txt(&bot, chat_id, &pool).await?;
                    return Ok::<_, teloxide::RequestError>(());
                }

                if text.starts_with("/check") {
                    let url_to_check = text.replace("/check", "").trim().to_string();
                    if url_to_check.is_empty() {
                        bot.send_message(chat_id, "Использование: /check <ссылка>").await?;
                    } else {
                        check_single_link(&bot, chat_id, &pool, &url_to_check).await?;
                    }
                    return Ok::<_, teloxide::RequestError>(());
                }

                let links: Vec<String> = url_regex.find_iter(&text).map(|m| m.as_str().to_string()).collect();
                
                if links.is_empty() {
                    bot.send_message(chat_id, "Пока пусто. Скидывай URL, или используй /export, /check <ссылка>").await?;
                    return Ok::<_, teloxide::RequestError>(());
                }

                let mut response = String::new();
                for link in links {
                    let existing: Option<(DateTime<Utc>,)> = sqlx::query_as("SELECT added_at FROM links WHERE url = $1")
                        .bind(&link)
                        .fetch_optional(&*pool)
                        .await
                        .ok()
                        .flatten();

                    if let Some(date) = existing {
                        response.push_str(&format!("❌ Дубль знайдено!: {} (была добавлена {})\n", link, date.0.format("%Y-%m-%d %H:%M")));
                    } else {
                        sqlx::query("INSERT INTO links (url) VALUES ($1)")
                            .bind(&link)
                            .execute(&*pool)
                            .await
                            .ok();
                        response.push_str(&format!("Сохранено: {}\n", link));
                    }
                }

                bot.send_message(chat_id, response).await?;
                Ok(())
            }
        },
    ));

    Dispatcher::builder(bot, handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

async fn check_single_link(bot: &Bot, chat_id: ChatId, pool: &PgPool, url: &str) -> Result<(), teloxide::RequestError> {
    let existing: Option<(DateTime<Utc>,)> = sqlx::query_as("SELECT added_at FROM links WHERE url = $1")
        .bind(url)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();

    let response = if let Some(date) = existing {
        format!("❌ Эта ссылка уже на сайте. Была добавлена: {}", date.0.format("%Y-%m-%d %H:%M"))
    } else {
        format!("✅ Такой ссылки в базе нет. Отправь её мне сообщением.")
    };

    bot.send_message(chat_id, response).await?;
    Ok(())
}

async fn export_links_to_txt(bot: &Bot, chat_id: ChatId, pool: &PgPool) -> Result<(), teloxide::RequestError> {
    let links: Result<Vec<(String,)>, _> = sqlx::query_as("SELECT url FROM links ORDER BY added_at DESC")
        .fetch_all(pool)
        .await;

    match links {
        Ok(rows) => {
            if rows.is_empty() {
                bot.send_message(chat_id, "База ссылок пуста.").await?;
                return Ok(());
            }

            let file_content = rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>().join("\n");
            let file_path = "links_export.txt";
            
            if let Ok(mut file) = File::create(file_path).await {
                let _ = file.write_all(file_content.as_bytes()).await;
                bot.send_document(chat_id, InputFile::file(file_path)).caption("Все ссылки из базы").await?;
            } else {
                bot.send_message(chat_id, "Ошибка создания файла").await?;
            }
        }
        Err(e) => {
            eprintln!("DB error export: {}", e);
            bot.send_message(chat_id, "Ошибка DB при выгрузке").await?;
        }
    }
    Ok(())
}

fn is_safe_url(url_str: &str) -> bool {
    if let Ok(url) = reqwest::Url::parse(url_str) {
        if let Some(host_str) = url.host_str() {
            let host = host_str.to_lowercase();
            
            if host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "0.0.0.0" {
                return false;
            }
            
            if host.starts_with("10.") || 
               host.starts_with("192.168.") || 
               host.starts_with("169.254.") || 
               host.starts_with("172.16.") || host.starts_with("172.17.") || 
               host.starts_with("172.18.") || host.starts_with("172.19.") ||
               host.starts_with("172.2") || host.starts_with("172.3") {
                return false;
            }
            
            return true;
        }
    }
    false
}

async fn check_404_links(bot: &Bot, pool: &PgPool, allowed_users: &[i64]) -> Result<(), teloxide::RequestError> {
    let links: Vec<(String,)> = sqlx::query_as("SELECT url FROM links")
        .fetch_all(pool)
        .await
        .unwrap_or_default();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();

    for (url,) in links {
        if !is_safe_url(&url) {
            eprintln!("Skipping unsafe/internal URL during ping: {}", url);
            continue;
        }

        if let Ok(resp) = client.get(&url).send().await {
            let status = resp.status();
            if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
                for user_id in allowed_users {
                    let msg = format!("Внимание!!! 404!!! Ссылка выдает 404 ошибку:\n{}\n\n", url);
                    bot.send_message(ChatId(*user_id), msg).await?;
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }
    Ok(())
}
