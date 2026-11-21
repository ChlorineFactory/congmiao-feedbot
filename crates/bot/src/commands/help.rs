//! `/help`, `/start` and the login usage page.

use feedbot_core::{Extractor, StoredCredentialMeta};
use feedbot_extractors::ExtractorRegistry;
use teloxide::types::InlineKeyboardMarkup;
use teloxide::utils::command::BotCommands;
use teloxide::utils::html;

use super::{Command, account::platform_login_hint, unknown_platform};
use crate::ui;

pub fn help_overview(registry: &ExtractorRegistry) -> String {
    render_overview(registry, None)
}

pub(crate) fn help_overview_with_accounts(
    registry: &ExtractorRegistry,
    credentials: &[StoredCredentialMeta],
    private: bool,
) -> String {
    render_overview(registry, Some((credentials, private)))
}

fn render_overview(
    registry: &ExtractorRegistry,
    accounts: Option<(&[StoredCredentialMeta], bool)>,
) -> String {
    let cmds = Command::bot_commands()
        .into_iter()
        .map(|cmd| {
            let name = cmd.command.trim_start_matches('/');
            ui::cmd_line(&format!("/{name}"), &cmd.description)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let platforms = if registry.all().is_empty() {
        "（暂无）".into()
    } else {
        ui::join_blocks(
            &registry
                .all()
                .iter()
                .map(|ext| {
                    let mut block = ui::platform_block(ext.as_ref());
                    append_login_hint(&mut block, ext.as_ref(), accounts);
                    block
                })
                .collect::<Vec<_>>(),
        )
    };
    ui::compose(
        "CongMiaoFeedBot",
        Some("发送链接即可解析。"),
        &[
            ui::section_expandable("命令", &cmds),
            ui::section("平台", &platforms),
        ],
    )
}

pub fn help_platform_page(
    registry: &ExtractorRegistry,
    name: &str,
) -> (String, Option<InlineKeyboardMarkup>) {
    render_platform_page(registry, name, None)
}

pub(crate) fn help_platform_page_with_accounts(
    registry: &ExtractorRegistry,
    name: &str,
    credentials: &[StoredCredentialMeta],
    private: bool,
) -> (String, Option<InlineKeyboardMarkup>) {
    render_platform_page(registry, name, Some((credentials, private)))
}

fn render_platform_page(
    registry: &ExtractorRegistry,
    name: &str,
    accounts: Option<(&[StoredCredentialMeta], bool)>,
) -> (String, Option<InlineKeyboardMarkup>) {
    let Some(ext) = registry.resolve_name(name) else {
        return (unknown_platform(name, registry), None);
    };
    (
        render_platform(ext.as_ref(), accounts),
        Some(ui::help_platform_keyboard(ext.id(), ext.auth().is_some())),
    )
}

pub fn help_platform(registry: &ExtractorRegistry, name: &str) -> String {
    let Some(ext) = registry.resolve_name(name) else {
        return unknown_platform(name, registry);
    };
    render_platform(ext.as_ref(), None)
}

fn render_platform(
    ext: &dyn Extractor,
    accounts: Option<(&[StoredCredentialMeta], bool)>,
) -> String {
    let id = ext.id();
    let lead = if let Some(auth) = ext.auth() {
        let methods: Vec<_> = auth.methods().iter().map(|m| m.to_string()).collect();
        format!("登录方式：{}", methods.join(", "))
    } else {
        "无需登录".into()
    };

    let mut cmds = Vec::new();
    if ext.auth().is_some() {
        cmds.push(ui::cmd_line(&format!("/login {id}"), "登录（仅私聊）"));
        cmds.push(ui::cmd_line(
            "/accounts",
            "查看登录状态（群聊显示全局登录）",
        ));
        cmds.push(ui::cmd_line(
            &format!("/logout {id}"),
            "注销个人账号（仅私聊）",
        ));
    }
    cmds.push(ui::cmd_line(&format!("/set {id}"), "查看或修改默认参数"));
    cmds.push(ui::cmd_line(
        &format!("/sub {}", ui::placeholder("<链接>")),
        "订阅该来源",
    ));

    let mut kinds_body = String::new();
    let kinds = ext.extract_kinds();
    let urls = ext.url_examples();
    if !kinds.is_empty() {
        kinds_body.push_str(&html::escape(&kinds.join(" / ")));
    }
    if !urls.is_empty() {
        if !kinds_body.is_empty() {
            kinds_body.push('\n');
        }
        kinds_body.push_str("链接：\n");
        kinds_body.push_str(
            &urls
                .iter()
                .map(|u| ui::placeholder(u))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    let mut sections = vec![ui::section("命令", &cmds.join("\n"))];
    if !kinds_body.is_empty() {
        sections.push(ui::section("解析类型", &kinds_body));
    }
    sections.push(ui::section_expandable("参数", &ext.options().help_table()));

    let mut out = ui::title_with_id(ext.display_name(), id);
    out.push('\n');
    out.push_str(&ui::lead(&lead));
    append_login_hint(&mut out, ext, accounts);
    for s in sections {
        out.push_str(&s);
    }
    out
}

fn append_login_hint(
    text: &mut String,
    ext: &dyn Extractor,
    accounts: Option<(&[StoredCredentialMeta], bool)>,
) {
    if let Some((credentials, private)) = accounts
        && ext.auth().is_some()
    {
        let hint = platform_login_hint(credentials, ext.id(), private);
        text.push('\n');
        text.push_str(&ui::lead(&hint));
    }
}

pub fn usage_login(registry: &ExtractorRegistry) -> String {
    let logins: Vec<_> = registry
        .all()
        .iter()
        .filter(|e| e.auth().is_some())
        .collect();
    let platforms = if logins.is_empty() {
        "当前没有需要登录的平台。".into()
    } else {
        ui::join_blocks(
            &logins
                .iter()
                .map(|ext| ui::platform_block(ext.as_ref()))
                .collect::<Vec<_>>(),
        )
    };
    ui::compose(
        "登录",
        Some("私聊选择平台即可开始。"),
        &[
            ui::section(
                "用法",
                &format!("/login {}\n仅私聊可用", ui::placeholder("<平台>")),
            ),
            ui::section("可登录平台", &platforms),
            ui::section(
                "账户管理",
                &[
                    ui::cmd_line("/accounts", "查看已保存的登录及状态"),
                    ui::cmd_line("/logout", "选择要注销的个人账号"),
                ]
                .join("\n"),
            ),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use feedbot_core::{AuthMethod, CredentialStatus, Scope};

    #[test]
    fn help_shows_global_status_and_keeps_personal_status_private() {
        let registry = ExtractorRegistry::builtin();
        let credentials = [
            (Scope::User, 10, CredentialStatus::Expired),
            (Scope::Global, 0, CredentialStatus::Valid),
        ]
        .map(|(scope, scope_id, status)| StoredCredentialMeta {
            extractor_id: "pixiv".into(),
            scope,
            scope_id,
            method: AuthMethod::Cookie,
            expires_at: None,
            status,
        });

        for private in [true, false] {
            let overview = help_overview_with_accounts(&registry, &credentials, private);
            let (platform, _) =
                help_platform_page_with_accounts(&registry, "pixiv", &credentials, private);
            assert!(overview.contains("全局登录（有效）"));
            assert!(platform.contains("全局登录（有效）"));
            assert_eq!(overview.contains("已过期"), private);
            assert_eq!(platform.contains("已过期"), private);
        }

        let (unconfigured, _) =
            help_platform_page_with_accounts(&registry, "x", &credentials, false);
        assert!(unconfigured.contains("全局登录（未配置）"));
    }
}
