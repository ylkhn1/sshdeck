//! Пароли баз данных в системном хранилище паролей (Secret Service: KWallet,
//! GNOME Keyring и т. п.). На диск приложение их не пишет.
//!
//! Пароль привязан к серверу и пользователю, а не к названию подключения и
//! не к базе: переименование подключения или выбор другой базы на том же
//! сервере его не теряют.

use std::collections::HashMap;

use super::profile::DbProfile;

const APPLICATION: &str = "sshdeck";
const KIND: &str = "database";

/// Ключ пароля: СУБД, пользователь, сервер, порт и SSH-сервер.
pub fn key(profile: &DbProfile) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        profile.kind.id(),
        profile.user,
        profile.host,
        profile.port,
        profile.ssh
    )
}

fn attributes(key: &str) -> HashMap<&'static str, String> {
    HashMap::from([
        ("application", APPLICATION.to_string()),
        ("kind", KIND.to_string()),
        ("server", key.to_string()),
    ])
}

fn describe(err: oo7::Error) -> String {
    format!("Хранилище паролей недоступно: {err}")
}

async fn keyring() -> Result<oo7::Keyring, String> {
    let keyring = oo7::Keyring::new().await.map_err(describe)?;
    // Заблокированное хранилище система попросит открыть сама.
    if keyring.is_locked().await.unwrap_or(false) {
        keyring.unlock().await.map_err(describe)?;
    }
    Ok(keyring)
}

/// Сохранённый пароль; `None` — его нет.
pub async fn load(key: String) -> Result<Option<String>, String> {
    let keyring = keyring().await?;
    let items = keyring
        .search_items(&attributes(&key))
        .await
        .map_err(describe)?;
    let Some(item) = items.into_iter().next() else {
        return Ok(None);
    };
    if item.is_locked().await.unwrap_or(false) {
        item.unlock().await.map_err(describe)?;
    }
    let secret = item.secret().await.map_err(describe)?;
    Ok(Some(
        String::from_utf8_lossy(secret.as_bytes()).into_owned(),
    ))
}

/// Сохраняет (или заменяет) пароль. `label` видно в менеджере паролей.
pub async fn store(key: String, label: String, password: String) -> Result<(), String> {
    let keyring = keyring().await?;
    keyring
        .create_item(&label, &attributes(&key), password, true)
        .await
        .map_err(describe)
}

/// Удаляет пароль; отсутствие пароля ошибкой не считается.
pub async fn forget(key: String) -> Result<(), String> {
    let keyring = keyring().await?;
    keyring.delete(&attributes(&key)).await.map_err(describe)
}

/// Подпись записи в менеджере паролей.
pub fn label(profile: &DbProfile) -> String {
    format!("sshdeck: {} ({})", profile.name, profile.summary())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbKind;

    #[test]
    fn key_ignores_name_and_database() {
        let base = DbProfile {
            name: "Прод".to_string(),
            host: "db".to_string(),
            user: "app".to_string(),
            database: "shop".to_string(),
            ..DbProfile::template(DbKind::Postgres)
        };
        let renamed = DbProfile {
            name: "Другое".to_string(),
            database: "other".to_string(),
            ..base.clone()
        };
        assert_eq!(key(&base), key(&renamed));
        assert_eq!(key(&base), "postgres|app|db|5432|");
        let other_user = DbProfile {
            user: "root".to_string(),
            ..base.clone()
        };
        assert_ne!(key(&base), key(&other_user));
    }

    /// Настоящее хранилище паролей: временная запись создаётся, читается
    /// и удаляется. Запуск: `cargo test -- --ignored live_keyring`.
    #[test]
    #[ignore = "пишет во временную запись настоящего хранилища паролей"]
    fn live_keyring_round_trip() {
        let key = "sshdeck-selftest|temporary".to_string();
        let password = "p@$$w0rd:;'\"\\юникод".to_string();
        let label = "sshdeck selftest (можно удалить)".to_string();
        crate::db::spawn(store(key.clone(), label, password.clone()))
            .recv_blocking()
            .expect("result")
            .expect("store");
        let loaded = crate::db::spawn(load(key.clone())).recv_blocking();
        assert_eq!(loaded, Ok(Ok(Some(password))));
        crate::db::spawn(forget(key.clone()))
            .recv_blocking()
            .expect("result")
            .expect("forget");
        let after = crate::db::spawn(load(key)).recv_blocking();
        assert_eq!(after, Ok(Ok(None)));
    }
}
