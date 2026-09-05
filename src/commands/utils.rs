use std::sync::Arc;

use serenity::all::*;
use serenity::model::id::UserId;

use tokio::sync::RwLock;

use crate::cache::BotCache;

// 💡 필요한 컨텍스트를 구조체로 묶습니다.
pub struct ChannelManager<'a> {
    pub ctx: &'a Context,
    pub guild_id: GuildId,
    pub command: &'a CommandInteraction,
    pub cache_lock: Arc<RwLock<BotCache>>,
}

// 하위 명령어 옵션에서 특정 이름의 문자열 값을 가져오는 함수
pub fn get_string_arg<'a>(opt: &'a CommandDataOption, name: &str) -> Option<String> {
    if let CommandDataOptionValue::SubCommand(sub_opts) = &opt.value {
        return sub_opts.iter().find(|o| o.name == name).and_then(|o| {
            if let CommandDataOptionValue::String(val) = &o.value {
                Some(val.clone())
            } else {
                None
            }
        });
    }
    None
}

// 명령어가 작성된 카테고리 ID를 가져오는 함수
// 프로젝트 카테고리 내부 채널에서만 명령어를 실행할 수 있도록 검증
pub async fn get_project_category_id(
    channel_manager: &ChannelManager<'_>,
    error_message: &str,
) -> Option<ChannelId> {
    let (ctx, command) = (channel_manager.ctx, channel_manager.command);

    let check_category = async {
        let Ok(Channel::Guild(guild_ch)) = command.channel_id.to_channel(&ctx.http).await else {
            return None;
        };
        let parent_id = guild_ch.parent_id?;
        let Ok(Channel::Guild(parent_ch)) = parent_id.to_channel(&ctx.http).await else {
            return None;
        };

        if parent_ch.kind == ChannelType::Category {
            Some(parent_id)
        } else {
            None
        }
    };

    if let Some(category_id) = check_category.await {
        return Some(category_id);
    }

    let _ = command
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new().content(error_message),
        )
        .await;

    None
}

// 문자열을 Discord UserId로 변환하는 함수
pub fn convert_string_to_discord_id(id_str: &str) -> Option<UserId> {
    let user_id = match id_str.parse::<u64>() {
        Ok(id) => UserId::new(id),
        Err(_) => return None,
    };

    Some(user_id)
}

// 프로젝트 pm인지 확인
pub async fn is_project_pm(channel_manager: &ChannelManager<'_>, category_id: ChannelId) -> bool {
    let (ctx, command, cache_lock) = (
        channel_manager.ctx,
        channel_manager.command,
        &channel_manager.cache_lock,
    );

    // 프로젝트 이름 가져오기
    let project_name = match category_id.to_channel(&ctx.http).await {
        Ok(Channel::Guild(cat_channel)) => cat_channel.name,
        _ => String::new(),
    };

    // 프로젝트 이름으로 프로젝트 정보 가져오기
    let project = match BotCache::get_project_by_name(cache_lock, &project_name).await {
        Some(proj) => proj,
        None => return false,
    };

    // 프로젝트 PM인지 확인
    project.pm.discord_id == command.user.id.to_string()
}

pub async fn is_server_admin(channel_manager: &ChannelManager<'_>) -> bool {
    let (ctx, command, guild_id) = (
        channel_manager.ctx,
        channel_manager.command,
        channel_manager.guild_id,
    );

    let guild = match guild_id.to_partial_guild(&ctx.http).await {
        Ok(guild) => guild,
        Err(_) => return false,
    };

    return guild.owner_id == command.user.id;
}
