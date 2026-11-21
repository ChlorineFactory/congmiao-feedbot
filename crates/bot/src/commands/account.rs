//! `/logout` and `/accounts`.

use feedbot_core::{CredentialStatus, Scope, StoreError, StoredCredentialMeta};
use feedbot_extractors::ExtractorRegistry;
use teloxide::prelude::*;
use teloxide::types::InlineKeyboardMarkup;
use teloxide::utils::html;

use super::{is_private, unknown_platform, user_id};
use crate::app::App;
use crate::ui::{self, UiCallback};

pub(super) async fn logout_usage(
    bot: &crate::app::TgBot,
    msg: &Message,
    app: &App,
) -> anyhow::Result<()> {
    let list = app.store.list_for_user(user_id(msg)).await?;
    let (text, kb) = logout_page(&app.registry, &list);
    ui::send_page(bot, msg.chat.id, text, kb).await
}

fn logout_page(
    registry: &ExtractorRegistry,
    list: &[StoredCredentialMeta],
) -> (String, Option<InlineKeyboardMarkup>) {
    let personal: Vec<_> = list
        .iter()
        .filter(|meta| meta.scope == Scope::User)
        .collect();
    let text = ui::compose(
        "退出登录",
        None,
        &[
            ui::section("用法", &format!("/logout {}", ui::placeholder("<平台>"))),
            credential_section(registry, "个人登录", &personal),
        ],
    );
    (text, logout_keyboard(registry, &personal))
}

/// Delete the credential stored at `(scope, scope_id)` for platform `name`.
pub(crate) async fn logout_html(
    app: &App,
    name: &str,
    (scope, scope_id): (Scope, i64),
) -> anyhow::Result<(String, Option<InlineKeyboardMarkup>)> {
    let Some(ext) = app.registry.resolve_name(name) else {
        return Ok((unknown_platform(name, &app.registry), None));
    };
    let ok = app
        .store
        .delete_credential(ext.id(), scope, scope_id)
        .await?;
    let scope_label = if scope == Scope::Global {
        "共享"
    } else {
        "个人"
    };
    let (heading, lead) = if ok {
        (
            "已退出登录",
            format!("{} 的{scope_label}凭据已移除。", ext.display_name()),
        )
    } else {
        (
            "未找到凭据",
            format!("{} 没有保存{scope_label}凭据。", ext.display_name()),
        )
    };
    Ok((
        ui::compose(
            heading,
            Some(&lead),
            &[ui::section("下一步", "/accounts 查看登录状态")],
        ),
        Some(ui::help_platform_keyboard(ext.id(), ext.auth().is_some())),
    ))
}

pub(super) async fn accounts(
    bot: &crate::app::TgBot,
    msg: &Message,
    app: &App,
) -> anyhow::Result<()> {
    let viewer = is_private(msg).then(|| user_id(msg));
    let list = visible_credentials(app, viewer).await?;
    let (text, kb) = accounts_page(&app.registry, &list, viewer.is_some());
    ui::send_page(bot, msg.chat.id, text, kb).await
}

pub(super) async fn visible_credentials(
    app: &App,
    viewer: Option<i64>,
) -> Result<Vec<StoredCredentialMeta>, StoreError> {
    match viewer {
        Some(user_id) => app.store.list_for_user(user_id).await,
        None => app.store.list_global().await,
    }
}

fn accounts_page(
    registry: &ExtractorRegistry,
    list: &[StoredCredentialMeta],
    private: bool,
) -> (String, Option<InlineKeyboardMarkup>) {
    let shared: Vec<_> = list
        .iter()
        .filter(|meta| meta.scope == Scope::Global)
        .collect();
    if !private {
        return (
            ui::compose(
                "登录状态",
                Some("私聊发送 /accounts 查看个人登录。"),
                &[credential_section(registry, "全局登录", &shared)],
            ),
            None,
        );
    }
    let personal: Vec<_> = list
        .iter()
        .filter(|meta| meta.scope == Scope::User)
        .collect();
    let mut sections = vec![credential_section(registry, "个人登录", &personal)];
    if !shared.is_empty() {
        sections.push(credential_section(registry, "全局登录", &shared));
    }
    let kb = if personal.is_empty() {
        let platforms = ui::named_platforms(registry.all(), true);
        ui::platform_keyboard(UiCallback::Login, &platforms)
    } else {
        logout_keyboard(registry, &personal)
    };
    (ui::compose("登录状态", None, &sections), kb)
}

pub(super) fn platform_login_hint(
    credentials: &[StoredCredentialMeta],
    platform_id: &str,
    private: bool,
) -> String {
    let status = |scope| {
        credentials
            .iter()
            .find(|meta| meta.scope == scope && meta.extractor_id == platform_id)
            .map_or("未配置", |meta| status_label(meta.status))
    };
    let global = status(Scope::Global);
    if private {
        format!("个人登录（{}） · 全局登录（{global}）", status(Scope::User))
    } else {
        format!("全局登录（{global}）")
    }
}

fn credential_section(
    registry: &ExtractorRegistry,
    heading: &str,
    entries: &[&StoredCredentialMeta],
) -> String {
    if entries.is_empty() {
        return ui::section(heading, "没有保存的登录。");
    }
    let blocks: Vec<_> = entries
        .iter()
        .map(|meta| {
            let id = ui::placeholder(&meta.extractor_id);
            let platform = match registry.by_id(&meta.extractor_id) {
                Some(ext) => format!("{} ({id})", html::escape(ext.display_name())),
                None => id,
            };
            format!(
                "{platform} · {} · {}",
                meta.method,
                status_label(meta.status)
            )
        })
        .collect();
    ui::section(heading, &ui::join_blocks(&blocks))
}

fn logout_keyboard(
    registry: &ExtractorRegistry,
    personal: &[&StoredCredentialMeta],
) -> Option<InlineKeyboardMarkup> {
    if personal.is_empty() {
        return None;
    }
    let rows = personal
        .iter()
        .map(|meta| {
            let ext = registry.by_id(&meta.extractor_id);
            let name = ext
                .as_ref()
                .map_or(meta.extractor_id.as_str(), |e| e.display_name());
            vec![(
                format!("退出 {name}"),
                UiCallback::Logout(meta.extractor_id.clone()),
            )]
        })
        .collect();
    Some(ui::keyboard(rows))
}

fn status_label(status: CredentialStatus) -> &'static str {
    match status {
        CredentialStatus::Valid => "有效",
        CredentialStatus::Expired => "已过期",
        CredentialStatus::Invalid => "已失效",
        CredentialStatus::Unknown => "状态未知",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feedbot_core::AuthMethod;

    #[test]
    fn account_pages_show_status_and_offer_logout_only_for_personal_credentials() {
        let registry = ExtractorRegistry::builtin();
        let entries = [
            ("x", Scope::User, CredentialStatus::Expired, 10),
            ("x", Scope::Global, CredentialStatus::Valid, 0),
            ("pixiv", Scope::Global, CredentialStatus::Unknown, 0),
        ]
        .map(|(id, scope, status, scope_id)| StoredCredentialMeta {
            extractor_id: id.into(),
            scope,
            scope_id,
            method: AuthMethod::Cookie,
            expires_at: None,
            status,
        });
        let expected_keyboard = Some(ui::keyboard(vec![vec![(
            "退出 X".into(),
            UiCallback::Logout("x".into()),
        )]]));

        let (accounts, keyboard) = accounts_page(&registry, &entries, true);
        let (personal, shared) = accounts.split_once("全局登录").unwrap();
        assert!(personal.contains("个人登录"));
        assert!(personal.contains("已过期"));
        assert!(shared.contains("有效"));
        assert!(shared.contains("状态未知"));
        assert_eq!(keyboard, expected_keyboard);

        let (group_accounts, keyboard) = accounts_page(&registry, &entries, false);
        assert!(group_accounts.contains("全局登录"));
        assert!(group_accounts.contains("状态未知"));
        assert!(!group_accounts.contains("已过期"));
        assert!(keyboard.is_none());
        let (unconfigured, _) = accounts_page(&registry, &entries[..1], false);
        assert!(unconfigured.contains("没有保存的登录"));

        let (logout, keyboard) = logout_page(&registry, &entries);
        assert!(logout.contains("已过期"));
        assert!(!logout.contains("pixiv"));
        assert_eq!(keyboard, expected_keyboard);
        assert!(logout_page(&registry, &entries[1..]).1.is_none());
    }
}
