use tracing::{error, info};

use serenity::all::{
    ChannelId, CommandOptionType, CreateCommand, CreateCommandOption, GuildId, Permissions,
};
use serenity::builder::{CreateChannel, EditChannel, EditInteractionResponse, EditRole};
use serenity::model::application::CommandInteraction;
use serenity::model::channel::{
    Channel, ChannelType, PermissionOverwrite, PermissionOverwriteType,
};
use serenity::model::id::RoleId;
use serenity::prelude::*;

use crate::cache::*;
use crate::commands::utils::*;
use crate::integration::notion::{member::*, project::*};

pub async fn run_project_command(
    ctx: &Context,
    command: &CommandInteraction,
) -> serenity::Result<()> {
    command.defer(&ctx.http).await?;

    // 서버 내에서만 명령어 실행 가능하도록 검증
    let Some(guild_id) = command.guild_id else {
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .content("❌ 이 명령어는 서버 안에서만 사용 가능합니다."),
            )
            .await?;
        return Ok(());
    };

    // 첫 번째 옵션에서 서브커맨드 추출 [generate, rename, delete]
    let Some(subcommand_option) = command.data.options.first() else {
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new().content("❌ 올바른 하위 명령어를 선택해주세요."),
            )
            .await?;
        return Ok(());
    };

    // 공유 캐시 및 알림 채널 불러오기
    let data_read = ctx.data.read().await;
    let cache_lock = data_read
        .get::<crate::cache::SharedCacheKey>()
        .expect("보관함에 캐시가 없습니다.")
        .clone();

    // let tx = data_read
    //     .get::<CacheNotifyKey>()
    //     .expect("보관함에 캐시 갱신 신호가 없습니다.")
    //     .clone();

    // 필요한 컨텍스트를 구조체로 묶기
    let channel_manager = ChannelManager {
        ctx,
        guild_id,
        command,
        cache_lock,
        // tx,
    };

    // 서브커맨드 이름 매칭 분기
    match subcommand_option.name.as_str() {
        "generate" => {
            // 서브커맨드 하위에 포함된 인자(name) 추출
            let project_name = match get_string_arg(subcommand_option, "name") {
                Some(name) => name,
                None => {
                    command
                        .edit_response(
                            &ctx.http,
                            EditInteractionResponse::new()
                                .content("⚠️ 생성할 프로젝트 이름을 제대로 입력해주세요."),
                        )
                        .await?;
                    return Ok(());
                }
            };

            if find_already_exist_project_name(&channel_manager, &project_name).await {
                return Ok(());
            }

            generate_project(&channel_manager, project_name).await?;
        }

        "rename" => {
            // 서브커맨드 하위에 포함된 인자(new_name) 추출
            let new_name = match get_string_arg(subcommand_option, "new_name") {
                Some(name) => name,
                None => {
                    command
                        .edit_response(
                            &ctx.http,
                            EditInteractionResponse::new()
                                .content("⚠️ 변경할 새 이름을 입력해주세요."),
                        )
                        .await?;
                    return Ok(());
                }
            };

            let error_msg = "❌ 프로젝트 카테고리 내부 채널에서 명령어를 입력해주세요.";
            let Some(category_id) = get_project_category_id(&channel_manager, error_msg).await
            else {
                return Ok(());
            };

            // 프로젝트 PM만 이름 변경 가능하도록 검증
            if is_project_pm(&channel_manager, category_id).await == false {
                command
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new()
                            .content("❌ 프로젝트 PM만 이름 변경이 가능합니다."),
                    )
                    .await?;
                return Ok(());
            }

            let old_name = match category_id.to_channel(&ctx.http).await {
                Ok(Channel::Guild(cat_channel)) => cat_channel.name,
                _ => String::new(),
            };

            if find_already_exist_project_name(&channel_manager, &new_name).await {
                return Ok(());
            }

            rename_project(&channel_manager, category_id, old_name, new_name).await?;
        }

        "delete" => {
            let guild = guild_id.to_partial_guild(&ctx.http).await?;
            let Some(member) = &command.member else {
                return Ok(());
            };

            let error_msg = "❌ 삭제할 프로젝트 카테고리 내부의 채널에서 명령어를 입력해주세요.";
            let Some(category_id) = get_project_category_id(&channel_manager, error_msg).await
            else {
                return Ok(());
            };

            let is_admin = guild.owner_id == command.user.id
                || member.roles.iter().any(|role_id| {
                    guild
                        .roles
                        .get(role_id)
                        .map_or(false, |r| r.permissions.administrator())
                });

            if !is_admin {
                command
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new().content(
                            "❌ 이 명령어는 서버 관리자(ADMINISTRATOR) 권한이 필요합니다.",
                        ),
                    )
                    .await?;
                return Ok(());
            }

            delete_project(&channel_manager, category_id).await?;
        }
        _ => {}
    }

    Ok(())
}

async fn generate_project(
    channel_manager: &ChannelManager<'_>,
    project_name: String,
) -> serenity::Result<()> {
    let (ctx, guild_id, command, cache_lock) = (
        channel_manager.ctx,
        channel_manager.guild_id,
        channel_manager.command,
        &channel_manager.cache_lock,
    );

    command
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new().content(format!(
                "🏗️ '{}' 프로젝트 생성을 시작합니다. 세팅 중...",
                project_name
            )),
        )
        .await?;

    let Ok(bot_user) = ctx.http.get_current_user().await else {
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new().content("❌ 봇 정보를 가져오지 못했습니다."),
            )
            .await?;
        return Ok(());
    };

    let Ok(project_role) = guild_id
        .create_role(&ctx.http, EditRole::new().name(&project_name))
        .await
    else {
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new().content("❌ 역할 생성 실패"),
            )
            .await?;
        return Ok(());
    };

    let _ = ctx
        .http
        .add_member_role(guild_id, command.user.id, project_role.id, None)
        .await;

    // 카테고리 및 채널 생성을 위한 권한 묶음 가져오기
    let overwrites =
        build_permission_overwrites(guild_id, project_role.id, bot_user.id, command.user.id);

    let category_builder = CreateChannel::new(&project_name)
        .kind(ChannelType::Category)
        .permissions(vec![
            overwrites.deny_everyone.clone(),
            overwrites.allow_project.clone(),
            overwrites.allow_bot.clone(),
        ]);

    let Ok(category) = guild_id.create_channel(&ctx.http, category_builder).await else {
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new().content("❌ 카테고리 생성 실패"),
            )
            .await?;
        return Ok(());
    };

    // 텍스트 채널 생성
    let text_channels = [
        "📜information",
        "🤖bot",
        "🌐dev",
        "🚩issue",
        "✅progress",
        "📢notice",
        "🎡random",
        "🖥️github",
    ];
    for ch_name in text_channels {
        let mut builder = CreateChannel::new(ch_name)
            .kind(ChannelType::Text)
            .category(category.id);

        builder = match ch_name {
            "🖥️github" => builder.permissions(vec![
                overwrites.deny_everyone.clone(),
                overwrites.readonly_project.clone(),
                overwrites.allow_bot.clone(),
            ]),
            "📜information" => builder.permissions(vec![
                overwrites.deny_everyone.clone(),
                overwrites.readonly_project.clone(),
                overwrites.allow_pm.clone(),
                overwrites.allow_bot.clone(),
            ]),
            _ => builder,
        };

        let _ = guild_id.create_channel(&ctx.http, builder).await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // 음성 채널 생성
    let voice_builder = CreateChannel::new("🎙️ voice chat")
        .kind(ChannelType::Voice)
        .category(category.id);
    let _ = guild_id.create_channel(&ctx.http, voice_builder).await;

    command
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new().content(format!(
                "🚀 <@{}> 님, 프로젝트 서버 세팅이 완료되었습니다!",
                command.user.id
            )),
        )
        .await?;

    // Notion 프로젝트 생성
    let pm = match BotCache::get_member_by_user_id(&cache_lock, command.user.id).await {
        Some(member) => member,
        None => NotionMember::default(),
    };

    let project = Project {
        name: project_name.clone(),
        pm: pm.clone(),
        category_id: category.id.to_string(),
        ..Default::default()
    };

    if let Err(why) = create_project(&project).await {
        error!(
            "Notion 프로젝트 등록 실패: {}. 에러: {:?}",
            project_name, why
        );
    }

    Ok(())
}

async fn rename_project(
    channel_manager: &ChannelManager<'_>,
    category_id: ChannelId,
    old_name: String,
    new_name: String,
) -> serenity::Result<()> {
    let (ctx, command, cache_lock) = (
        channel_manager.ctx,
        channel_manager.command,
        &channel_manager.cache_lock,
    );

    // Notion 프로젝트 캐시에서 가져오기
    let mut notion_project = match BotCache::get_project_by_name(&cache_lock, &old_name).await {
        Some(project) => project.clone(),
        None => return Ok(()),
    };
    // notion_project.participants.push(notion_project.pm.clone());

    let builder = EditChannel::new().name(&new_name);
    if let Err(why) = category_id.edit(&ctx.http, builder).await {
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new().content(format!("❌ 이름 변경 실패: {:?}", why)),
            )
            .await?;
        return Ok(());
    }

    // Notion 프로젝트 이름 변경
    notion_project.name = new_name.clone();

    let role_renamed = try_rename_role(ctx, channel_manager.guild_id, &old_name, &new_name).await;
    let msg_content = if role_renamed {
        format!(
            "📝 프로젝트 카테고리와 역할 이름이 모두 '{}'으로 변경되었습니다.",
            new_name
        )
    } else {
        format!(
            "📝 프로젝트 이름은 '{}'으로 변경되었으나, 동명의 기존 역할을 찾지 못했습니다.",
            new_name
        )
    };

    command
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new().content(msg_content),
        )
        .await?;

    // Notion 동기화
    let project_id = notion_project.id.clone();
    match update_project(&project_id, &notion_project).await {
        Ok(_) => {
            info!(
                "Notion 프로젝트 '{}' 정보가 성공적으로 업데이트되었습니다.",
                new_name
            );
        }
        Err(why) => {
            error!(
                "Notion 프로젝트 '{}' 정보 업데이트 실패. 에러: {:?}",
                new_name, why
            );
        }
    }

    Ok(())
}

async fn delete_project(
    channel_manager: &ChannelManager<'_>,
    category_id: ChannelId,
) -> serenity::Result<()> {
    let (ctx, guild_id, command, cache_lock) = (
        channel_manager.ctx,
        channel_manager.guild_id,
        channel_manager.command,
        &channel_manager.cache_lock,
    );

    let project_name = match category_id.to_channel(&ctx.http).await {
        Ok(Channel::Guild(cat)) => cat.name,
        _ => String::new(),
    };

    command
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new()
                .content("🧹 프로젝트 채널들과 역할을 완전히 삭제합니다..."),
        )
        .await?;

    // 1. 하위 채널 청소
    if let Ok(channels) = guild_id.channels(&ctx.http).await {
        for (id, guild_channel) in &channels {
            if guild_channel.parent_id == Some(category_id) && *id != command.channel_id {
                let _ = guild_channel.id.delete(&ctx.http).await;
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        let _ = command.channel_id.delete(&ctx.http).await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // 2. 카테고리 삭제
    let _ = category_id.delete(&ctx.http).await;

    // 3. 동명 역할 찾아서 삭제
    try_delete_role(ctx, guild_id, &project_name).await;

    // 4. Notion 프로젝트 삭제
    let project_id = match BotCache::get_project_id_by_name(&cache_lock, &project_name).await {
        Some(id) => id,
        None => return Ok(()),
    };

    use crate::integration::notion::project::delete_project;
    match delete_project(&project_id).await {
        Ok(_) => {
            info!("Notion 프로젝트 '{}' 삭제 완료", project_name);
        }
        Err(why) => {
            error!(
                "Notion 프로젝트 '{}' 삭제 실패. 에러: {:?}",
                project_name, why
            );
        }
    }

    // // 완전 삭제까지 1초 대기 후 캐시 갱신
    // tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    Ok(())
}

// --- 헬퍼 함수들 ---

// 이미 존재하는 프로젝트 이름인지 확인하고, 존재하면 에러 메시지 전송
async fn find_already_exist_project_name(channel_manager: &ChannelManager<'_>, name: &str) -> bool {
    // 1. 키 존재 여부만 빠르게 확인 후 락 자동 해제
    let exists = channel_manager
        .cache_lock
        .read()
        .await
        .project_name_to_id
        .contains_key(name);

    // 2. 락이 풀린 상태에서 안전하게 HTTP 통신 진행
    if exists {
        let _ = channel_manager
            .command
            .edit_response(
                &channel_manager.ctx.http,
                EditInteractionResponse::new()
                    .content(format!("❌ 이미 '{}' 이름의 프로젝트가 존재합니다.", name)),
            )
            .await;
        return true;
    }

    false
}

// 역할 이름 변경 시도, 성공 여부 반환
async fn try_rename_role(ctx: &Context, guild_id: GuildId, old_name: &str, new_name: &str) -> bool {
    if old_name.is_empty() {
        return false;
    }
    let Ok(roles) = guild_id.roles(&ctx.http).await else {
        return false;
    };
    if let Some(role) = roles
        .values()
        .find(|r| r.name.eq_ignore_ascii_case(old_name))
    {
        return guild_id
            .edit_role(&ctx.http, role.id, EditRole::new().name(new_name))
            .await
            .is_ok();
    }
    false
}

// 역할 이름으로 역할 삭제 시도
async fn try_delete_role(ctx: &Context, guild_id: GuildId, role_name: &str) {
    if role_name.is_empty() {
        return;
    }
    if let Ok(roles) = guild_id.roles(&ctx.http).await {
        if let Some(role) = roles
            .values()
            .find(|r| r.name.eq_ignore_ascii_case(role_name))
        {
            let _ = guild_id.delete_role(&ctx.http, role.id).await;
        }
    }
}

// 프로젝트 카테고리 및 채널 생성 시 필요한 권한 오버라이트를 구조체로 묶어 반환
struct ProjectOverwrites {
    deny_everyone: PermissionOverwrite,
    allow_project: PermissionOverwrite,
    readonly_project: PermissionOverwrite,
    allow_pm: PermissionOverwrite,
    allow_bot: PermissionOverwrite,
}

fn build_permission_overwrites(
    guild_id: GuildId,
    project_role_id: RoleId,
    bot_id: serenity::model::id::UserId,
    pm_id: serenity::model::id::UserId,
) -> ProjectOverwrites {
    ProjectOverwrites {
        deny_everyone: PermissionOverwrite {
            allow: Permissions::empty(),
            deny: Permissions::VIEW_CHANNEL,
            kind: PermissionOverwriteType::Role(RoleId::new(guild_id.get())),
        },
        allow_project: PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Role(project_role_id),
        },
        readonly_project: PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL,
            deny: Permissions::SEND_MESSAGES,
            kind: PermissionOverwriteType::Role(project_role_id),
        },
        allow_pm: PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Member(pm_id),
        },
        allow_bot: PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL
                | Permissions::SEND_MESSAGES
                | Permissions::MANAGE_CHANNELS,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Member(bot_id),
        },
    }
}

// 프로젝트 관련 슬래시 커맨드 등록
pub fn register_project_command() -> CreateCommand {
    CreateCommand::new("project")
        .description("프로젝트 관련 명령어")
        .dm_permission(false)
        .default_member_permissions(Permissions::MANAGE_CHANNELS)
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "generate",
                "새 프로젝트를 생성합니다.",
            )
            .add_sub_option(
                CreateCommandOption::new(CommandOptionType::String, "name", "생성할 프로젝트 이름")
                    .required(true),
            ),
        )
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "rename",
                "프로젝트 이름을 변경합니다.",
            )
            .add_sub_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "new_name",
                    "변경할 새 프로젝트 이름",
                )
                .required(true),
            ),
        )
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "delete",
            "프로젝트를 완전히 삭제합니다.",
        ))
}
